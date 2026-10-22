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
| Database | `.sqlite`, `.sqlite3`, `.db` whose header says SQLite | an attached catalog in the workspace (`ATTACH … (TYPE sqlite, BUSY_TIMEOUT 1000)`) | in place |
| Data | `.csv`, `.tsv`, `.txt`, `.csv.gz` | a view over `read_csv` | through Import |
| Data | `.parquet`, or a folder of them, partitions included | a view over `read_parquet('dir/**/*.parquet', hive_partitioning = true)` | through Import |
| Data | `.json`, `.ndjson`, `.jsonl` | a view over `read_json` | through Import |
| Data | `.xlsx` | a view over `read_xlsx`, its first sheet | through Import |
| Data | `.geojson`, `.shp`, `.gpkg` | a view over `ST_Read`, its first layer | through Import |

`excel`, `sqlite` and `spatial` are extensions the engine installs on first
use, once, from DuckDB's repository; all three load on the pinned engine
(measured 2026-10-10). `excel` and `sqlite` load themselves; `spatial` does
not, so opening a spatial file loads it, and a server that opens the
workspace loads it before it serves (below). DuckTable loads it too when the
catalog of a server that has not, a Harbor that does not or a database other
than the workspace, reports a view over `ST_Read` it cannot read; a catalog
whose views all read asks nothing more. A file whose kind cannot be told is
refused with what was looked at, never guessed at.

## The workspace

A data file has no database of its own, so it opens in the **workspace**:
one DuckDB database, Harbor's, at `~/.local/share/harbor/workspace.duckdb`
(`$HARBOR_HOME/workspace.duckdb` when `HARBOR_HOME` is set, else
`$XDG_DATA_HOME/harbor/` when that is). DuckTable reads its path from
`harbor_common::paths::workspace()`, the function the `harbor` command line
uses, so `harbor sales.csv` and a drop on DuckTable land in the same place,
and a view made by either is there for both. Harbor serves it like any other
database, and DuckTable lists it first in the sidebar as **Workspace**,
running or not, once it exists. Opening a file there:

- creates a view named for the file's stem (`sales.csv` → `sales`, a second
  `sales` → `sales_2`; a reserved word or a name a table or view already has
  takes the next number) that reads the file where it lies, so a 10 GB
  Parquet file opens at once and the grid reads only the page it shows;
- records the path and the reader in the view's own SQL, so the workspace
  outlives a restart and a file that has moved says so in the sidebar, the
  engine's words in the view's tooltip;
- attaches a SQLite file and writes its row in the workspace's record, so
  the attachment outlives the server (below);
- finds, on a second opening of the same file, the view or the attachment
  the first made, and says it is already in the workspace;
- leaves the file untouched. Nothing DuckTable does writes to a data file.

A SQLite file is attached under a name made the same way, one no database,
schema or reserved word has, nor one the record keeps for another file. It
is attached read-write, its tables edited in place with the grid's rules
(EDITING.md), keyed by their primary key or by rowid and hash. SQLite
refuses `INSERT … RETURNING`, so an insert there is checked by its count.

An `ATTACH` lasts only as long as the server that ran it, so the workspace
records its attachments in itself: the table `_harbor.attached` (`name`,
`path`, `type`, `read_only`), made where it is not by
`wire::open::RECORD_DDL`, in a schema no view or catalog name made here can
take. Whoever attaches a file writes its row as it attaches it
(`Record::insert_sql`: the CLI, and DuckTable in one place,
`workspace::record_attachment`), which keeps it through a server that does
not stop cleanly; and a server serving the workspace rewrites the record
from what is attached when it stops, so an `ATTACH` or `DETACH` from any
client lasts. The record is Harbor's, and `/catalog` does not list it.

Every server that opens the workspace makes it whole before it serves, for
any client: it loads `spatial` when a view calls an `ST_` function, since
`spatial` does not load on demand (`excel`, `json`, `parquet` and `sqlite`
do), and attaches again each file the record names. A recorded file that is
gone at the start is not attached, since attaching a missing path makes an
empty database there; it is listed among `attached` with no tables and its
reason in `error`, and its row stays, so the file is attached again once it
is back. DuckTable shows it as a greyed heading, the reason in its
tooltip.

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
it is. **Detach** on an attached catalog's heading detaches it with
`DETACH IF EXISTS` and deletes its row from the record (`Record::delete_sql`),
so no later server attaches it again; what was staged on its tables is
parked with the rest of the workspace's. A greyed heading gets the same
`DETACH`: a busy file is still attached, and a file the server set aside is
forgotten once a client detaches its name, while one never attached
detaches nothing. In a database other than the workspace, Detach is the
`DETACH` alone.

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
- **The workspace is whole at every start**: the extension its views call
  into loaded and its recorded files attached before the server serves, as
  above.

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
