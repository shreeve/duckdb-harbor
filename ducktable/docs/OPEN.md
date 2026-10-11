# Opening anything

How DuckTable and the `harbor` command line open a file that is not a
DuckDB database: a CSV, a folder of Parquet, a spreadsheet, a SQLite file.
DuckDB reads all of them; DuckTable and the CLI make that a drag, a
double-click or one word.

## What opens, and as what

A file is recognized by its first bytes where it has a signature, and by its
extension where it does not. Two kinds follow from that:

| Kind | Files | Opens as | Editable |
|---|---|---|---|
| Database | `.duckdb`, `.ddb`, and `.db` whose header says DuckDB | itself, served by Harbor | in place |
| Database | `.sqlite`, `.sqlite3`, `.db` whose header says SQLite | an attached catalog in the workspace (`ATTACH … (TYPE sqlite)`) | in place |
| Data | `.csv`, `.tsv`, `.txt`, `.csv.gz` | a view over `read_csv` | through Import |
| Data | `.parquet`, or a folder of them, partitions included | a view over `read_parquet('dir/**/*.parquet', hive_partitioning = true)` | through Import |
| Data | `.json`, `.ndjson`, `.jsonl` | a view over `read_json` | through Import |
| Data | `.xlsx` | a view over `read_xlsx`, one per sheet | through Import |
| Data | `.geojson`, `.shp`, `.gpkg` | a view over `ST_Read` | through Import |

`excel`, `sqlite` and `spatial` are extensions the engine installs on first
use, once, from DuckDB's repository; all three load on the pinned engine
(measured 2026-10-10). A file whose kind cannot be told is refused with what
DuckTable looked at, never guessed at.

## The workspace

A data file has no database of its own, so it opens in the **workspace**:
one DuckDB database, Harbor's, at `~/.local/share/harbor/workspace.duckdb`
(`$XDG_DATA_HOME/harbor/` or `$HARBOR_HOME` when set), served by Harbor like
any other and shown first in the sidebar as **Workspace**. DuckTable reads
its path from `harbor_common::paths::workspace()`, the function the `harbor`
command line uses, so `harbor sales.csv` and a drop on DuckTable land in the
same place, and a view made by either is there for both. Opening a file
there:

- creates a view named for the file's stem (`sales.csv` → `sales`, a second
  `sales` → `sales_2`) that reads the file where it lies, so a 10 GB Parquet
  file opens at once and the grid reads only the page it shows;
- records the path and the reader in the view's own SQL, so the workspace
  outlives a restart and a file that has moved says so when it is opened;
- attaches a SQLite file and writes its row in the workspace's record,
  `_harbor.attached` (`name`, `path`, `type`, `read_only`; the statements
  are `wire::open::RECORD_DDL` and `Record::insert_sql`), so the attachment
  outlives the server;
- leaves the file untouched. Nothing DuckTable does writes to a data file.

Views in the workspace can be joined with each other, queried in the Query
view, sorted, filtered and exported like any table. Two actions turn them
into data DuckTable can edit and keep:

- **Import as Table** copies a view into a table of the same name
  (`CREATE TABLE … AS SELECT * FROM …`), editable with the grid's rules, and
  drops the view. A large file asks first, saying the size.
- **Save As…** writes a table or view back out, to CSV, Parquet, JSON or
  Excel, through the Export path (`COPY … TO`).

Remove on a workspace entry drops the view or table; it never touches the
file. A SQLite file is attached in the workspace read-write, and its tables
are edited in place with the same rules as a DuckDB table; Detach removes the
attachment, with a plain `DETACH`.

Every server that opens the workspace makes it whole before it serves, for
any client: it loads `spatial` when a view calls an `ST_` function, since
`spatial` does not load on demand (`excel`, `json`, `parquet` and `sqlite`
do), and attaches again each file the record names. When it stops it
rewrites the record from what is attached then, so an `ATTACH` or `DETACH`
from any client lasts; writing the row at the attach keeps it through a
server that does not stop cleanly. The record is Harbor's, and `/catalog`
does not list it. A recorded file that is gone at the start is listed among
`attached` with no tables and its reason in `error`; DuckTable shows it so,
greyed, with the reason, and Detach on it deletes the row
(`DELETE FROM _harbor.attached WHERE name = …`), since there is nothing
attached to detach.

## Every way in

All of these land in one function that classifies the path and opens it:

- **Finder**: double-click, or Open With. The bundle declares document types
  for every extension above (`CFBundleDocumentTypes` in the bundle's
  `Info.plist`), so macOS offers DuckTable and hands the paths to GPUI's
  `on_open_urls`.
- **The Dock**: dropping files on the icon arrives the same way.
- **The window**: dropping files anywhere on it (`ExternalPaths`), which
  today opens only a `.duckdb` file and only the first path. Every path in a
  drop opens; a drop of several data files opens them all in the workspace
  and selects the first.
- **The command line**: `ducktable sales.csv data/` and
  `open -a DuckTable sales.csv`.
- **The menu**: File → Open… takes any of the kinds above.

Opening a data file while another database is connected switches to the
Workspace, asking first only about what cannot be parked, as any switch does.

## Harbor's half

Three server changes make the workspace possible:

- **`/catalog` lists views and attached catalogs.** It lists tables in the
  current database only (`viewsSomeday` is the placeholder). Views come with
  their SQL, their columns and no row count (a count would read the file);
  attached databases come as further catalogs with their schemas. DuckTable
  shows them in the TABLES tree, views marked read-only. This is the
  "views and macros in the sidebar" item, pulled forward for views and
  attachments; macros follow later.
- **The command line opens files too.** `harbor sales.csv` opens the REPL on
  `~/.local/share/harbor/workspace.duckdb` with a view `sales` over the file,
  the same rule DuckTable uses, so `SELECT * FROM sales` works at once.
  `harbor x.duckdb` is unchanged; only paths that classify as data or SQLite
  go to the workspace.
- **The workspace is whole at every start**: the extensions its views need
  loaded and its recorded files attached, before the server serves, as
  above.

A workspace server is never `--sealed`: reading files and loading
extensions is its job. A sealed server refuses `read_csv` and `INSTALL`, and
DuckTable says so instead of failing obscurely.

## Not in this round

- A remote database's workspace. Files are read on the machine the server
  runs on, so dropping a local file onto a remote database cannot work as a
  view; DuckTable opens it in the local workspace instead.
- Live MySQL or Postgres servers (`ATTACH … (TYPE postgres)`): a database URL
  feature of its own.
- Macros in the sidebar.

## Order of work

1. Harbor: `/catalog` views and attached catalogs (with `wire::catalog`),
   and `harbor <data file>`; tests in the catalog suite.
2. DuckTable: the classifier (a pure, tested function), the workspace
   database and its views, the TABLES tree's views and attachments, Import
   as Table.
3. DuckTable: every way in — document types, `on_open_urls`, multi-path
   drops, command-line paths, File → Open….
4. Save As… arrives with the export feature, which is being built now.
