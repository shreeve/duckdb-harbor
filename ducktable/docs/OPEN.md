# Opening anything

How DuckTable and the `harbor` command line open a file that is not a
DuckDB database: a CSV, a folder of Parquet, a spreadsheet, a SQLite file.
DuckDB reads all of them; DuckTable and the CLI make that a drag, a
double-click or one word.

## What opens, and as what

A file is recognized by its first bytes where it has a signature, and by its
extension where it does not (`wire::open::classify`, one rule for DuckTable
and the CLI). Two kinds follow from that:

| Kind | Files | Opens as | Editable |
|---|---|---|---|
| Database | `.duckdb`, `.ddb`, and `.db` whose header says DuckDB | itself, served by Harbor | in place |
| Database | `.sqlite`, `.sqlite3`, `.db` whose header says SQLite | an attached catalog in the workspace (`ATTACH … (TYPE sqlite)`) | in place |
| Data | `.csv`, `.tsv`, `.txt`, `.csv.gz` | a view over `read_csv` | through Import |
| Data | `.parquet`, or a folder of them, partitions included | a view over `read_parquet('dir/**/*.parquet', hive_partitioning = true)` | through Import |
| Data | `.json`, `.ndjson`, `.jsonl` | a view over `read_json` | through Import |
| Data | `.xlsx` | a view over `read_xlsx`, its first sheet | through Import |
| Data | `.geojson`, `.shp`, `.gpkg` | a view over `ST_Read`, its first layer | through Import |

`excel`, `sqlite` and `spatial` are extensions the engine installs on first
use, once, from DuckDB's repository; all three load on the pinned engine
(measured 2026-10-10). `excel` and `sqlite` load themselves; `spatial` does
not, so opening a spatial file loads it, and DuckTable loads it again when
the catalog of a server that has not (a workspace server started since)
reports a view over `ST_Read` it cannot read. A file whose kind cannot be
told is refused with what was looked at, never guessed at.

## The workspace

A data file has no database of its own, so it opens in the **workspace**:
the DuckDB database `harbor <data file>` opens too, at
`~/.local/share/harbor/workspace.duckdb` (`$HARBOR_HOME/workspace.duckdb`
under a `HARBOR_HOME`; `paths::workspace`). One workspace serves the CLI and
DuckTable, so a file opened in either is there in the other. Harbor serves
it like any other database, and DuckTable lists it first in the sidebar as
**Workspace**, running or not, once it exists. Opening a file there:

- creates a view named for the file's stem (`sales.csv` → `sales`, a second
  `sales` → `sales_2`; a reserved word or a name a table or view already has
  takes the next number) that reads the file where it lies, so a 10 GB
  Parquet file opens at once and the grid reads only the page it shows;
- records the path and the reader in the view's own SQL, so the workspace
  outlives a restart and a file that has moved says so in the sidebar, the
  engine's words in the view's tooltip;
- finds, on a second opening of the same file, the view the first made, and
  says it is already in the workspace;
- leaves the file untouched. Nothing DuckTable does writes to a data file.

A SQLite file is attached under a name made the same way, one no database,
schema or reserved word has. It is attached read-write, its tables edited in
place with the grid's rules (EDITING.md), keyed by their primary key or by
rowid and hash. SQLite refuses `INSERT … RETURNING`, so an insert there is
checked by its count. The attachment lasts as long as the workspace server;
recording it for the server's next start is the workspace's own (harbor's
start-up), and DuckTable writes that record in one place
(`workspace::record_attachment`).

Views in the workspace can be joined with each other, queried in the Query
view, sorted, filtered and exported like any table. A view's grid is
read-only, with no Add Row and nothing to stage, and the footer says it is a
view; it is paged without a total, since counting a view over a file reads
the whole file. Two actions, on a view's right-click menu in the sidebar,
turn it into data DuckTable keeps or sends on:

- **Import as Table** copies a workspace view into a table of the same name,
  editable with the grid's rules, in one transaction: the rows into a table
  under a free name, the view dropped, the table renamed to the view's name.
  A failure leaves the view as it was. A file over 100 MB asks first, saying
  its size, since the copy reads all of it into the workspace.
- **Save As** writes any table or view, of any database on this Mac, to CSV,
  Parquet, JSON or Excel through Export (`COPY … TO`), every row, under the
  same rules as File → Export Table.

**Remove from Workspace** on a workspace view drops it; the file is left as
it is. **Detach** on an attached catalog's heading detaches it, and what was
staged on its tables is parked with the rest of the workspace's.

## Every way in

All of these land in one function (`open::open_paths`) that reads what each
path holds, off the UI thread, and opens it:

- **Finder**: double-click, or Open With. The bundle declares a document
  type for every kind above (`CFBundleDocumentTypes` in the bundle's
  `Info.plist`, written by `scripts/macos-app.sh`), so macOS offers DuckTable
  and hands the paths to GPUI's `on_open_urls`. DuckTable owns DuckDB's
  files (`.duckdb`, `.ddb`); it is an alternate for CSV, TSV, JSON, Excel,
  GeoJSON, SQLite and folders, so they stay with the app that has them, and
  the default only where the system knows no type (Parquet, shapefiles,
  GeoPackages). Types the system does not declare are imported by their
  extensions.
- **The Dock**: dropping files on the icon arrives the same way.
- **The window**: dropping files anywhere on it (`ExternalPaths`). Every
  path in a drop opens.
- **The command line**: `ducktable sales.csv data/`, and
  `open -a DuckTable sales.csv`, which arrives as Finder's does. AppKit's
  `-Key value` arguments are passed over.
- **The menu**: File → Open… (⌘O) takes any of the kinds above, files and
  folders, several at once.

URLs that arrive before the window is up wait for it. Files that open in the
workspace open there together and the first is shown: its view, or the first
table of a SQLite file. A DuckDB database opens by itself, since a window
shows one database; one among data files is said not to have opened, as is
a second database. What opened is said in the status line; what did not, in
an alert, one paragraph a file.

Opening a data file while another database is connected switches to the
Workspace, asking first only about what cannot be parked, as any switch does
(`leave_asking`). On the Workspace already, the files open where it is, and
the table on screen gives way as any table switch makes it.

## Harbor's half

- **`/catalog` lists views and attached catalogs.** Views come with their
  SQL, their columns and no row count (a count would read the file), and
  with the engine's error when they do not read; attached databases come as
  further catalogs with their tables and views. DuckTable shows them in the
  TABLES tree: the database's own tables, then its views, then each attached
  catalog under its own heading.
- **The command line opens files too.** `harbor sales.csv` opens the REPL on
  the workspace with a view `sales` over the file, the same rule DuckTable
  uses, so `SELECT * FROM sales` works at once. `harbor x.duckdb` is
  unchanged; only paths that classify as data or SQLite go to the workspace.

A workspace server is never `--sealed`: reading files and loading
extensions is its job. A sealed server refuses `read_csv` and `INSTALL`, and
the engine's refusal is what DuckTable says.

## Not in this round

- A remote database's workspace. Files are read on the machine the server
  runs on, so a local file never opens on a remote database; it opens in the
  local workspace.
- A sheet other than an Excel workbook's first, and a layer other than a
  spatial file's first.
- Live MySQL or Postgres servers (`ATTACH … (TYPE postgres)`): a database URL
  feature of its own.
- Macros in the sidebar.
