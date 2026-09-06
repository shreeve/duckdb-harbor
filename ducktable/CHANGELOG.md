# DuckTable changelog

DuckTable release tags use `ducktable-vX.Y.Z`. Entries are ordered by signed
tag date, newest first.

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
