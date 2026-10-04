# DuckTable changelog

DuckTable release tags use `ducktable-vX.Y.Z`. Entries are ordered by release
date, newest first.

## 0.22.9 — 2026-10-03

- **⌘Q and the close button ask before they lose anything.** EDITING.md
  promised one dialog, on quitting with staged changes, and none existed: ⌘Q
  quit outright and the close button dropped every staged edit. ⌘Q, DuckTable
  → Quit and the close button now ask when changes are staged, in the table on
  screen or parked for another, when a cell editor is open, when a commit or a
  Query statement is still in flight, or when the Query view holds a
  transaction open, and say which. Cancel is the default: Return and Esc go
  back. The button that quits is not a tab stop, so no key reaches it and only
  a click on it quits. The dialog is the app's own because the platform alert
  cannot do that. Measured: with Cancel first it has no default button, and
  Space presses the button that discards. Three quits do not pass through the
  dialog and ask nothing: Quit from the Dock's menu, a logout or restart, and
  the updater's Install and Relaunch.
- **Every quit gives its sessions back.** Whether it asked or not, a quit
  releases the sessions the window holds, a commit's and an opening `BEGIN`'s
  among them, so the server rolls back at once what would otherwise wait for
  a timeout.
- **Nothing moves behind the quit dialog.** The menu bar and its keys still
  reach the app while the dialog is up. Opening, stopping or removing a
  database, switching tables or views, refreshing and the row commands are
  ignored until it is answered; a connect in flight is called off when it
  opens, and a table switch or a stopped server that lands under it waits for
  the answer. Cancel finds the grid, the query and the staged edits as they
  were.
- **The Query view holds a transaction.** Each run was its own request, so
  `BEGIN` answered and did nothing, the statements after it committed one by
  one, and `ROLLBACK` failed. A statement that begins a transaction now gets a
  Harbor session and every later run goes through it until `COMMIT`, `END`,
  `ROLLBACK` or `ABORT`, when the session goes back. The header band shows a
  transaction is open and how long the server will keep it: five minutes from
  `BEGIN`. The view touches the session every ten seconds so Harbor's
  thirty-second idle timeout does not take it between statements, releases it
  when the connection drops, another database is chosen or the app quits, and
  says so when the server has reclaimed it: the statement run next is refused
  once, since on its own it would commit on its own. The statement is read
  by the reader Harbor's server and its own client use (`wire::statement`),
  past the spaces the engine skips and no others, and through `EXPLAIN
  ANALYZE`, which measured runs the `COMMIT` it explains. Only the engine's
  answer ends a transaction: a `COMMIT` Harbor turns away because the session
  is still busy leaves it open. It works with a Harbor that refuses a `BEGIN` sent outside a
  session and with one that does not: the view never sends one.
- **A row connects to the database it shows.** A database file whose name
  matched a remote in the config showed one row with the local path, and a
  click on it looked the name up again, found the remote, and opened SSH: the
  grid and its edits went to the other host. A local row now connects by its
  file and a remote row by its config entry, the two are listed as two rows
  with a tooltip saying which is which, Retry dials what failed and not its
  name, and the sidebar's table counts join a running server without ever
  starting one or opening a tunnel. Rows are told apart by database and not
  by name throughout, two files with one stem included: the highlight, the
  spinner, and Stop, which shuts down the server of the row's own file where
  it used to stop whichever server answered to the name. A socket named for a
  database is joined only when the server behind it reports that file.
- **A commit whose answer is lost is not called a failure.** A timeout or a
  dropped tunnel on the `COMMIT` request was reported as "edits kept", though
  the server may have committed, and a second ⌘S then inserted every new row
  again. Now the commit's session is ended and seen to be over before the page
  is read, so the page is the commit's outcome, and the staged set is held: it
  is taken off the page, whose rows its old keys may no longer name, and
  nothing sends it, stages into it or undoes it. The count opens the review,
  where the whole set takes one of two verdicts, since a commit is all or
  nothing: it landed, and all of it is dropped for good, or it did not, and
  all of it is staged again. No single change is discarded from a held set:
  with a DELETE of 7 and a re-key of 3 to 7 held, dropping the re-key the page
  shows and sending the DELETE would delete the re-keyed row. No verdict is
  taken while the commit may still be running or before a page has been read
  after it. A `COMMIT` that could not be sent at all is an ordinary failure,
  edits kept.
- **A COMMIT that rolled back says so.** In the Query view an error aborts the
  transaction, and a `COMMIT` after it answers like any other while rolling
  everything back: the view showed `ok` and the work was gone. That included
  errors the user never saw, such as the count behind a paged result. The
  band now reads `transaction aborted by an error`, from the session's own
  answer to a question asked right after any statement that may have failed
  there, so it is never a guess. The view asks again in the same turn as the
  `COMMIT`, with nothing able to come between, and a `COMMIT` of an aborted
  transaction reports the rollback; one whose state cannot be confirmed is not
  sent. A `COMMIT` Harbor cancelled or failed on after the engine had it is in
  doubt, not open. A `ROLLBACK` the engine answers with "no transaction is
  active" is reported as that, not as a rollback.
- **Nothing is staged against a page from before the commit.** Between a
  commit and its refetch the grid showed the new values under the old row
  identities, so an edit staged then could name a row by a key the commit had
  just changed. The grid stays committing until the page lands; if the fetch
  fails it says so, keeps the ghosts of the rows the commit deleted, and takes
  no edits until a refresh.
- **A list or struct cell refuses an escape the engine does not read back.**
  The cell shows a newline as `\n`; typed back, the engine's cast stored the
  letter n, with no error. Measured: `'["line\nbreak"]'::VARCHAR[]` is
  `linenbreak`. Typed text for a container can escape only a quote and a
  backslash, and anything else is refused with a pointer to the Query tab.
- **A UNION cell takes no typed text.** `8` typed over `{"tag":"n","value":7}`
  in a `UNION(n INTEGER, s VARCHAR)` was stored as the string `8` under `s`.
- **A DECIMAL is held to its width in the editor.** `12345.6` into a
  `DECIMAL(5,2)` passed the editor and failed at commit, taking the whole
  transaction with it. The editor now refuses what the engine would, rounding
  as the engine rounds: `999.994` is taken and `999.995` is not.
- **⌃⇧N leaves a row staged for deletion alone.** It replaced the staged
  DELETE with an UPDATE to NULL.
- **A page change keeps what was being typed.** A pager click, a page-size
  change, a filter or a switch to another table confirmed nothing and dropped
  an open editor's text. They now confirm it first, as a refresh does, and ⌘S
  from the review popover does too.
- **A result cut short is not a result.** An answer that ended before its
  closing line, because the server or the connection died mid-stream, was
  taken as complete with the rows that had arrived. It is now a failure that
  says so.
- **More results page.** A statement behind a block comment, and `DESCRIBE`,
  `SUMMARIZE`, `SHOW`, `PIVOT` and `UNPIVOT`, ran unpaged and fetched their
  whole result. Numbers right-align by their own type name, so an INTERVAL and
  an `INTEGER[]` no longer do.
- The live probes in `harbor-client/tests/live.rs` connect only to a
  database file named for them: `HARBOR_LIVE_DB`, a scratch database, for all
  but the linger probe, which starts its own copy of the file `HARBOR_FIXTURE`
  names. They create and drop tables, and picking any running database could
  have picked the wrong one. They take turns, so the suite passes at any
  `--test-threads`.
- **A release uploads the feed last.** `scripts/release.sh` sends the update
  feed's archives and deltas first and `appcast.xml` after them. A feed that
  was up before the archive it names sent every client that checked in
  between to a 404.

## 0.22.8 — 2026-10-03

- **DuckTable runs on GPUI Kit 0.7.** The UI stack moves from GPUI 0.2.2 and
  gpui-component 0.5.1 to `gpui-kit` 0.7.0, which pins one weekly snapshot of
  Zed's GPUI and carries the component library with it. Several behaviors
  DuckTable had patched in are upstream now (selection painting, Shift-arrow
  keeping its column, the SQL highlighter's merge), so the vendored patches
  shrink from 51 marked sites in one crate to 26 in two, listed in the
  workspace `Cargo.toml`. The wide-table probe holds the 16.7ms frame on every
  phase, as before.
- **Starting an edit never moves a column.** Opening a cell editor or adding a
  row widened columns to fit their draft hints, pushing the whole row sideways.
  A new row's untouched cells are blank now, because the database fills them;
  the one mark is a soft red `REQUIRED` tag, holding only `!` in a column too
  narrow for the word.
- **The column card.** Hovering a new row's cell, or editing any cell, shows
  the column's type, its default or generation, whether it is required, and an
  ENUM's values. While editing, clicking a value picks it. ⌘T (View → Column
  Tooltips) turns the card off and on.
- **Esc dismisses an empty new row.** ⌘N then Esc leaves nothing behind; a row
  with a value entered stays, and ⌘⌫ discards a new row either way. ⌘Z brings
  a dismissed row back.
- **Refreshing an empty table holds still.** The loading placeholder no longer
  flashes over a table that is already on screen.
- **Both dividers look alike.** The inspector's divider drops its gripper and
  matches the sidebar's plain hairline.
- **Signed with a Developer ID, notarized, and on Homebrew.** DuckTable
  releases the way Shotts and Transfer do: built, signed and notarized
  locally by `scripts/release.sh`, so Gatekeeper opens it however it was
  downloaded, and installable with `brew install --cask shreeve/tap/ducktable`.
  Updates still arrive through the same feed. If macOS asks once more for Local
  Network access, that is the new signature; allow it as before.
- Ships Sparkle 2.10.0, the current stable. The lockfile records
  `harbor-common` and `wire` at 0.43.5.

## 0.22.7 — 2026-09-25

- **Closing the window quits the app.** DuckTable has one window, and its
  menus act on that window, so after it closed the app lived on as a bare
  menu bar holding a Dock slot until ⌘Q. Now the last window closing is the
  quit.

## 0.22.6 — 2026-09-22

- **An edit to a keyless table cannot land on another row.** A table without a
  primary key is edited by DuckDB's rowid, and a rowid names a position, not a
  row: a checkpoint that compacts deleted rows renumbers the rest. Measured: the
  row with id 212880 was fetched at rowid 212880; after another client deleted
  a batch and checkpointed, rowid 212880 held the row with id 290000, so a
  staged UPDATE or DELETE hit that row and still passed the exactly-one check.
  A keyless page now fetches the rowid paired with a hash of the whole row,
  and every statement's WHERE checks both. A row that moved or changed since
  the fetch matches nothing, and the commit refuses with the edits kept.
- **A table switch during a commit waits for it.** Switching tables while a
  commit was in flight parked the staged set whose statements were already
  running. The commit landed, but coming back to the table brought those edits
  back as staged, and a second ⌘S inserted every new row twice. The switch now
  runs once the commit settles.
- **Nothing stages, undoes or discards during a commit.** ⌘Z right after ⌘S
  showed the edit undone while the commit wrote it anyway, and anything staged
  during the commit was erased when it succeeded. Undo and redo, delete, clear,
  ⌃⇧N, the discards and the open editor's Enter all wait for the commit to
  settle.

## 0.22.5 — 2026-09-21

- **A FLOAT cell takes a number a FLOAT can hold.** The editor held integers
  to their type's range and refused `1e309` for a DOUBLE, but let `3.5e38` and
  `1e39` into a FLOAT, where the engine refused them at commit and the whole
  transaction went with them. The editor applies the engine's own rule: the
  number is rounded to the nearest FLOAT, and one that rounds past the largest
  is out of range. `3.40282356e38` still rounds to the largest FLOAT and is
  taken; `3.4028236e38` is refused. `nan` and `inf` are taken as before.
- **Text typed into a BLOB cell is base64, always.** The literal `null` typed
  into any cell that is not text means SQL NULL, and a BLOB cell followed the
  rule; but `null`, `NULL` and `Null` are each four base64 characters — the
  bytes `9EE965`, `3542CB` and `36E965` — so a cell showing one of them could
  not be typed again, and typing it set NULL. A BLOB cell's text is never
  NULL; NULL is entered with ⌃⇧N, Delete, or by emptying the cell. Every other
  type keeps the rule, a `BLOB[]` among them.
- **Whitespace around a number is not a change.** ` 5` typed over an INTEGER
  that holds `5` was staged as an update of `5` to `5`. For every type but
  text, `JSON` and `ENUM`, text that differs from what the cell holds only by
  the whitespace around it is that cell unchanged, and against the fetched
  value (or what a duplicate copied) it drops the staged edit. The engine
  agrees for each of them: a padded number, date, list or `VARIANT` is the
  bare value, and a padded `UUID`, `BIT` or base64 is refused. Text, a `JSON`
  column and an `ENUM` store the padding, and are compared exactly.
- **⌘D says why it did nothing.** On a draft row, on a row staged for deletion,
  and with no row selected, Duplicate Row returned silently. The status line
  gives the reason: a new row is not in the database yet, so there is nothing
  to copy it from; the delete has to be discarded first; a row has to be
  selected.
- **The review popover names a duplicate's source.** A duplicate was listed as
  `new row`, like a row made with ⌘N. It reads `new row · copy of id = 5`, with
  every column of a composite key, and `rowid` for a table without a key.
- **A table altered elsewhere is noticed.** A grid took its columns and types
  from its first page and kept them, so after another client ran `ALTER TABLE
  … ALTER col TYPE`, or added or dropped a column, later pages were laid into
  the old columns and staged values were bound through the placeholders of
  the old types: base64 into a column that had become a BLOB went in bare, as
  its characters. Every fetched page's column names and types are compared
  with the grid's. With nothing staged the grid adopts the table as it is, as
  a first page would, and refetches the catalog. With edits staged they are
  not rebound: the page on screen stays, staging and ⌘S are refused, and the
  status line says the columns changed and the staged edits have to be
  discarded; when the last one is, the table loads as it is. A stash parked
  for a table that changed shape while off-screen is still dropped, and the
  status line now says so and how many.
- **Staged edits survive a failed fetch.** Returning to a table removed its
  parked edits and handed them to the new grid, and a grid whose first page
  failed to load has no staging layer to take them: they were gone. Such a
  grid keeps the stash, adopts it when a refresh brings the page, and hands it
  back to be parked again if another table is chosen first.
- EDITING.md states each of these, and gains a section on a table altered
  elsewhere.
- The lockfile records `harbor-common` and `wire` at 0.41.3.

## 0.22.4 — 2026-09-21

- **A table keyed by a FLOAT can be edited.** Harbor sends a FLOAT as the
  shortest decimal that names it, and a JSON number binds as a DOUBLE. In
  `WHERE "k" = ?` the column was widened to meet it, and 1.1 the FLOAT is not
  1.1 the DOUBLE: the UPDATE and the DELETE matched no row and the commit
  rolled back, and ⌘D reported its source row gone. Only a key both widths
  hold exactly, such as 0.5, worked. A FLOAT key binds through `?::FLOAT`.
- **An empty editor over NULL leaves NULL.** A text cell staged to NULL with
  ⌃⇧N and then confirmed — Enter, Enter, or a Tab run passing through — was
  staged as `''`. NULL and `''` both open an empty editor, so confirming one
  over a cell that holds NULL changes nothing. On a duplicate the same confirm
  rebound a copied NULL by value and pushed an undo step nobody could see, so
  one ⌘Z did not remove the row; a duplicate is one undo step again.
- **A copied cell typed back is the copy again.** A duplicate's cell that was
  typed over and then typed back to the text it showed was bound by value, so
  a DATE or a HUGEINT inside a `VARIANT` became a string or a DOUBLE under
  identical text. The cell remembers what it copied, and holding that text
  again it is read from the source row, unvalidated, like a cell never
  touched.
- **Document cells take strict JSON, 100 levels deep.** `NaN` and `Infinity`
  were let through because the engine reads them; but Harbor then sends
  `{"x":NaN}`, which is not JSON, and a client reads the whole document back
  as a string. They are refused. A cell that already holds one can still be
  opened and left. A document nests at most 100 levels, as in every
  first-party client, and one that is deeper is told so; it was told "is not
  JSON — text needs quotes".
- **A container of documents or blobs refuses typed text.** A typed edit of a
  `BLOB[]`, `BLOB[2]`, `VARIANT[]`, `JSON[]` or `STRUCT(v VARIANT, …)` cell
  bound the displayed text bare, and the commit succeeded with the inner value
  corrupted: base64 characters stored as the bytes, documents stored as
  strings. A `MAP(VARCHAR, BLOB)` cell's displayed text was refused by the
  engine, loudly, but text typed in the engine's own syntax (`{k=aGk=}`)
  corrupted it the same way. The editor refuses the text for all of them and
  names the Query tab. Opening and leaving such a cell, clearing it to NULL, and ⌘D
  work as before.
- **A duplicate whose source row is gone says to discard it.** The message
  said "refresh and retry", and no refresh helps: the draft keeps naming the
  row it copies. It says to discard that duplicate, with ⌘Z or from the
  review popover.
- EDITING.md says what a `JSON` column keeps (the text, character for
  character) apart from what a `VARIANT` keeps (the values, compact); that a
  typed edit of a `VARIANT` retypes SQL-written values through JSON; that a
  duplicate carries its source row's identity; and that a failed commit
  reports in the status line only, marking no row.
- **The live probes choose a local database, never a remote.** The ignored
  tests in `harbor-client/tests/live.rs` took the first row of the fleet
  survey, and a remote configured by url surveys as stopped and sorts first
  when no local server is up: on a Mac whose harbor config names a production
  host, `cargo test -- --ignored` would have tunnelled to it and run the
  probes' `DROP TABLE IF EXISTS` and `CREATE TEMP TABLE` there. The probes
  consider only rows with a local database file, and skip when there is none.
- The lockfile records `harbor-common` and `wire` at 0.41.1.

## 0.22.3 — 2026-09-20

- **Duplicate Row is an exact copy.** ⌘D rebound the source row's wire values,
  and the wire is narrower than the engine: a `VARIANT` crosses it as JSON, so
  a DATE written into a document from SQL came back a string, a DECIMAL a
  double, a HUGEINT short of digits; a `BLOB[]` came back as base64 text
  re-encoded; an INTERVAL, a MAP and a UNION failed or landed in the wrong
  member. A duplicate's untouched cell is no longer bound at all: the INSERT
  reads it from the source row in SQL
  (`INSERT INTO t (…) SELECT ?, "c2", "c3" FROM t WHERE <key> = ? RETURNING *`),
  so the copy is the value that was there, whatever its type. A cell typed
  into the draft, or one carrying a staged update from the source row, is
  bound as before. If the source row is gone at ⌘S the commit stops and says
  so, rather than inserting a row of NULLs.
- **The editor judges a type by its own name.** A test for `INT` anywhere in
  the type sent `INTERVAL`, `INTEGER[]`, `STRUCT(a INTEGER, …)` and an ENUM
  holding `'POINT'` down the integer path, where every edit was refused; the
  same looseness caught `DOUBLE[]`, `DECIMAL(10,2)[]` and `VARCHAR[]`. Types
  match by exact name. Integers are checked against their own range and bound
  as text past 64 bits, so `UBIGINT`, `HUGEINT` and `UHUGEINT` take their full
  width exactly.
- **A number is never staged as NULL.** `nan`, `inf`, `-inf` and their
  spellings typed into a DOUBLE or FLOAT cell were staged as SQL NULL with no
  word; they bind as the values they name. A number past a double's range is
  refused in the editor.
- **Clearing an ENUM or a UUID means NULL.** Both were treated as text, so
  Delete staged `''`, which is no value of either and could not commit.
- **The bundle signs as one name, and says why it wants the local network.**
  macOS files an app's Local Network decision under its signing identifier,
  and DuckTable's SSH tunnels count as DuckTable. Every build signs as
  `com.shreeve.ducktable`, the build and the release smoke test fail on any
  other, and the bundle carries an `NSLocalNetworkUsageDescription` for the
  consent prompt.
- **Installs swap by rename.** Both installers stage the new bundle beside the
  old one, rename the old aside and the new in, and remove the old one last,
  so a failed install leaves the DuckTable that was there; a swap interrupted
  between its renames is put right on the next run. `install.sh` verifies the
  download's signature before it swaps, registers the bundle with Launch
  Services, and takes `DUCKTABLE_DEST`.
- **The release's feed upload sends files, not the pruned folder.** Sparkle
  2.10.0's `generate_appcast` sets the archives it prunes from the feed aside
  in `old_updates/`, and the workflow uploaded `target/updates/*`: the 0.22.2
  run published the release, the signed feed and every archive, then met that
  directory and ended red. The upload takes the files only; 0.22.3 is the
  first release it ran green on.
- Ships Sparkle 2.10.0, the current stable. The lockfile records
  `harbor-common` and `wire` at 0.41.0.

## 0.22.2 — 2026-09-20

- **Editing a `VARIANT` cell keeps it a document.** The cell's JSON text was
  bound through a bare `?`, which the engine stores as a VARIANT *string*:
  the cell still looked like JSON, and every path into it read NULL, with no
  error. A typed edit, a new row and ⌘D all did it. `VARIANT` and `JSON`
  cells now bind through `?::JSON`, the same way Harbor sends them out and
  the way Rip writes a `VARIANT` (Rip binds a `JSON` column through a bare
  `?`), and text that is not JSON is refused in the editor
  — a string wants its quotes, `"Morel"`, as the cell shows it. A row already
  damaged is repaired by
  `UPDATE t SET doc = doc::VARCHAR::JSON::VARIANT WHERE variant_typeof(doc) = 'VARCHAR' AND json_valid(doc::VARCHAR) AND json_type(doc::VARCHAR) IN ('OBJECT', 'ARRAY')`.
- **Editing a `BLOB` cell keeps its bytes.** A BLOB shows as base64, and the
  base64 characters went back as the bytes. It now binds through
  `from_base64(?::VARCHAR)`, in the WHERE as well: on a table keyed by a BLOB,
  an edit could miss its row or, where another key's bytes spelled the first
  one's base64, change the wrong one.
- **Enter on an unchanged cell stages nothing.** Confirming the text a cell
  already holds skipped no validation, so tabbing through a DOUBLE holding NaN
  or a `JSON` cell holding `null` staged a NULL, and an integer wider than 64
  bits stopped a Tab run with an error.
- Ships Sparkle 2.10.0, up from 2.9.6, so the updater itself is current. It
  re-applies file system compression correctly when a delta update lands on
  macOS 27, stops leaking temporary files when a delta fails to apply, and
  needs macOS 12.0, which is already DuckTable's floor.
- Updates the shared `harbor-common` and `wire` lockfile entries to 0.41.0.
  DuckTable reads nothing that changed in them: a VARIANT cell, which Harbor
  has delivered as JSON text since 0.39.0, arrives as text either way and
  now shows as the JSON it is.

## 0.22.1 — 2026-09-11

- Uses Harbor's corrected shared path handling: native canonical paths remain
  intact for database identity and file operations, while display formatting
  hides Windows verbatim prefixes. Updates the shared `harbor-common` and
  `wire` lockfile entries to 0.36.1.

## 0.22.0 — 2026-09-11

- Selects many rows at once, with the grammar every macOS list uses, on the
  gutter and the body cells alike: a plain click selects one row, ⌘-click
  toggles a row in or out, ⇧-click selects the span from the anchor through
  the clicked row. One row is the lead — the ring, the inspector and ⌘D
  follow it — and ⌘⌫ stages a `DELETE` for every selected row. Esc clears the
  whole selection. ⇧-click in the body is a seat cell ranges would want;
  docs/EDITING.md records that if ranges ship, body ⇧-click moves to them and
  the gutter keeps the row span.
- Undoes a many-row gesture in one step: one ⌘Z takes back a ⌘⌫ over a
  selection and one ⌘⇧Z replays it, and ⌘⇧⌫ discard-all is one step for the
  same reason. Single-row paths are unchanged.
- Ships Sparkle 2.9.6, up from 2.9.4, so the updater itself is current.

## 0.21.1 — 2026-09-06

- The first version to arrive through Check for Updates rather than the
  installer. Also folds a nested `if` in the view switcher into one, which
  clippy had been asking for.

## 0.21.0 — 2026-09-06

- Updates itself: DuckTable → Check for Updates…, and a daily check once you
  say yes to the first-launch prompt. Sparkle, fed from the `ducktable-updates`
  GitHub release (docs/UPDATES.md).

## 0.20.4 — 2026-09-05

- Keeps the active-cell coordinates intact when a staged row intentionally
  suppresses the ordinary row-selection color.
- Makes Tab and Shift-Tab resolve their destination before staging the edited
  value, then reliably open that destination for continued editing.

## 0.20.3 — 2026-09-05

- Keeps cell editing continuous across Tab and Shift-Tab: DuckTable confirms
  the current value, moves with row-local wraparound, and immediately opens
  the destination cell for editing.

## 0.20.2 — 2026-09-05

- Fixes Tab and Shift-Tab while a cell editor is active: the edited value is
  confirmed and the active cell moves right or left instead of remaining in
  the input.
- Preserves row-local wraparound, so Tab from the final visible cell selects
  the first and Shift-Tab from the first selects the final cell.

## 0.20.1 — 2026-09-05

- Makes a sidebar table-name double-click select the table and switch directly
  to its Data view while a single click preserves the current view.
- Makes Tab and Shift-Tab wrap between the first and last visible cells of the
  current row while navigating or confirming an edit.
- Keeps content-fit columns compact during ordinary viewing and applies wider
  draft-placeholder minimums only while editing or displaying draft rows.

## 0.20.0 — 2026-09-05

- Adds **Duplicate Row** with Cmd+D as a staged insert that copies exact source
  values while leaving primary-key and generated columns to DuckDB.
- Keeps **Delete Row** on Cmd+Delete and preserves staged, reversible deletion.
- Makes **Refresh Tables** update both the catalog and the currently open Data
  grid while leaving Query results unchanged.
- Refreshes the current Data grid after every completed Query run so external
  mutations become visible immediately.

## 0.19.3 — 2026-09-04

- Refreshes `/catalog` and all sidebar row counts after a successful staged
  edit commit and after every completed Query run.
- Makes manual and automatic catalog refreshes newest-wins, preventing a slow
  older response from replacing fresher counts.

## 0.19.2 — 2026-09-04

- Gives `REQUIRED`, `DEFAULT`, `NULL`, and `GENERATED` draft-cell badges enough
  minimum column width to remain readable.
- Scales badge-fit widths with the application zoom level.

## 0.19.1 — 2026-09-04

- Adds **New Row** and **Delete Row** to the Edit menu with Cmd+N and Cmd+D.
- Adds **Refresh Tables** to the View menu with Cmd+R, sharing the sidebar
  refresh action.
- Keeps row deletion staged and reversible until the final save.

## 0.19.0 — 2026-09-04

- Adds new-row drafts at the top of the Data grid, including `REQUIRED`,
  `DEFAULT`, `NULL`, and `GENERATED` guidance.
- Stages inserts alongside updates and deletes with undo, discard, table
  switching, and one all-or-nothing Cmd+S transaction.
- Applies DuckDB defaults and generated expressions at commit, validates
  required values, and refetches committed rows from the database.

## 0.18.3 — 2026-09-03

- Holds one shared HTTP connection while a database is open, keeping an
  ephemeral Harbor server alive between one-shot requests.
- Releases the connection—and any managed SSH tunnel—when the final database
  connection clone closes.
- Hardens HTTP response reading across nonblocking `WouldBlock` boundaries.

## 0.18.2 — 2026-09-03

- Simplifies the database URL dialog's field and documentation from
  “Harbor port” to the clearer “Port.”
- Aligns the saved-connection wording across the dialog, README, and UI guide.

## 0.18.1 — 2026-09-03

- Fixes dialog foreground colors so text remains readable in Duck Dark and
  Midnight themes.

## 0.18.0 — 2026-09-03

- Renames the connection workflow to **Open Database URL…** and keeps it
  reliably reusable after a dialog is closed.
- Displays Harbor's exact catalog row counts instead of storage estimates.
- Improves dialog layering, focus, and dismissal behavior.

## 0.17.0 — 2026-09-03

- Adds database URL connections using a sidebar name, host, and port.
- Connects `localhost` directly and automatically creates an app-owned SSH
  tunnel for non-local hosts.
- Uses unattended OpenSSH with keepalives, user SSH configuration, automatic
  local-port selection, lifecycle monitoring, and cleanup on disconnect.

## 0.16.0 — 2026-09-03

- Aligns DuckTable's client, saved connections, tests, and documentation with
  Harbor's direct connection model.

## 0.15.3 — 2026-09-02

- Adds extra right-side breathing room for the caret in tightly fitted cell
  editors.

## 0.15.2 — 2026-09-02

- Makes display mode and edit mode paint cell text at the same position.
- Uses exact Menlo metrics and balanced editor padding when fitting columns.

## 0.15.0 — 2026-09-02

- Reconciles a database that exits while open back to an idle sidebar entry
  with click-to-reconnect behavior.
- Routes catalog-refresh failures through the same departed-server recovery
  path.

## 0.14.0 — 2026-09-02

- Adds **Open Database File…**, Cmd+O, and drag-and-drop opening.
- Adds right-click lifecycle actions, stop progress, departure animation, and
  a copyable database path.
- Adds one-click upgrades for local databases running an older Harbor binary,
  preserving their lifetime mode during restart.

## 0.11.1 — 2026-09-01

- Uses Harbor's canonical session endpoint for transaction-backed editing.
- Aligns the client with the root-server and `/sql` execution API.

## 0.11.0 — 2026-09-01

- Discovers Harbor databases from their listening sockets and starts missing
  local databases on demand.
- Joins supported Harbor socket layouts in one fleet view.
- Makes the installer choose the highest DuckTable version tag.

## 0.10.0 — 2026-09-01

- Moves DuckTable into the `duckdb-harbor` monorepo beside Harbor.
- Uses in-repository Harbor client and wire crates so both sides of the
  protocol compile together.
- Adds dedicated DuckTable CI and version-aware macOS release installation.
