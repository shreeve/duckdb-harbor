//! Export (docs/DESIGN.md, "Copy and export"): a table or a query's result
//! written to a file by the engine itself, `COPY (SELECT …) TO '<path>'`,
//! run through Harbor on a pooled connection. Nothing pages through the
//! client, so a table of any size exports at the engine's speed.
//!
//! The server writes the file, so the database must be one on this Mac:
//! a database reached by URL may be served from another machine, whose
//! disk the COPY would write to, and is refused. The COPY writes to a
//! hidden name beside the file asked for, which takes its place only once
//! the COPY has succeeded; a failure removes it. (The engine removes what
//! a failed CSV, Parquet or JSON COPY wrote, but a failed Excel COPY leaves
//! its partial file behind: measured.)
//!
//! The status line says how it goes (`footer::say`), the way it says what
//! a copy took.

use crate::footer::say;
use crate::util::qlit;
use gpui_kit::*;
use harbor_client::Conn;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The file formats an export writes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Format {
    #[default]
    Csv,
    Parquet,
    Json,
    Excel,
}

impl Format {
    fn extension(self) -> &'static str {
        match self {
            Format::Csv => "csv",
            Format::Parquet => "parquet",
            Format::Json => "json",
            Format::Excel => "xlsx",
        }
    }
}

/// What an export reads, from the grid it was asked of.
pub(crate) struct Plan {
    pub(crate) conn: Conn,
    /// The SELECT whose rows the file holds.
    pub(crate) select: String,
    /// The suggested file name, before its extension.
    pub(crate) stem: String,
}

/// Why this grid cannot be exported, or None when it can. `local` is a
/// database file on this Mac; `results` asks for the Query view's result,
/// which must be one the grid pages (a statement that reads as a subquery)
/// and must not have been read inside a transaction, whose writes a COPY
/// on another connection would not see.
pub(crate) fn refusal(local: bool, results: bool, pageable: bool, in_transaction: bool) -> Option<&'static str> {
    if !local {
        return Some(
            "export writes through the database's own server, and a database opened by URL may be \
             served from another machine: open its file on this Mac to export",
        );
    }
    if results && !pageable {
        return Some("this result cannot be exported: its statement is not a query");
    }
    if results && in_transaction {
        return Some(
            "this result was read inside a transaction, which an export would not see: \
             COMMIT, then run the statement again to export it",
        );
    }
    None
}

/// The SELECT an export reads: every column of `source` under the filter
/// strip's condition, in the order the grid shows, the header sort's with
/// its tie-break or else the strip's ORDER BY. These are the rows the grid
/// pages without the page's LIMIT (`sql::sorted_page_sql`, which a test
/// holds this to); a result's grid has no header sort, and passes none.
pub(crate) fn table_select(
    source: &str,
    filter: &Option<String>,
    sort: &[crate::sql::SortKey],
    tiebreak: &[String],
) -> String {
    crate::sql::ordered_sql(source, false, filter, sort, tiebreak)
}

/// The file name, lowercased, without a compression suffix the engine
/// reads from it (`.gz`, `.zst`).
fn bare_name(path: &Path) -> String {
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    for suffix in [".gz", ".zst"] {
        if let Some(bare) = name.strip_suffix(suffix) {
            return bare.to_string();
        }
    }
    name
}

/// The format a file is written in: its extension's, when it names one of
/// the four, so a name typed as `orders.parquet` under Export Table → CSV
/// is Parquet; otherwise the menu's.
pub(crate) fn format_for(path: &Path, chosen: Format) -> Format {
    let name = bare_name(path);
    match name.rsplit_once('.').map(|(_, ext)| ext) {
        Some("csv") => Format::Csv,
        Some("parquet") => Format::Parquet,
        Some("json" | "ndjson" | "jsonl") => Format::Json,
        Some("xlsx") => Format::Excel,
        _ => chosen,
    }
}

/// The COPY options for a format. CSV and Excel carry a header row; JSON
/// is one array of objects, or an object per line for a file named
/// `.ndjson` or `.jsonl`. A `.gz` or `.zst` name compresses, the engine's
/// own reading of the name.
pub(crate) fn copy_options(format: Format, path: &Path) -> &'static str {
    let lines = matches!(bare_name(path).rsplit_once('.').map(|(_, ext)| ext), Some("ndjson" | "jsonl"));
    match format {
        Format::Csv => "FORMAT csv, HEADER true",
        Format::Parquet => "FORMAT parquet",
        Format::Json if lines => "FORMAT json",
        Format::Json => "FORMAT json, ARRAY true",
        Format::Excel => "FORMAT xlsx, HEADER true",
    }
}

/// The statement that writes the file. The SELECT sits on lines of its
/// own, so one that ends in a line comment cannot reach the closing
/// parenthesis, and the path is a string literal, its quotes doubled.
pub(crate) fn copy_sql(select: &str, path: &str, options: &str) -> String {
    format!("COPY (\n{select}\n) TO {} ({options})", qlit(path))
}

/// The hidden name beside `path` that the COPY writes to. It ends with the
/// whole file name, so the engine reads the same compression from it.
pub(crate) fn temp_path(path: &Path, pid: u32, seq: u64) -> PathBuf {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!(".ducktable-export-{pid}-{seq}-{name}"))
}

/// The name the save panel suggests.
pub(crate) fn suggested_name(stem: &str, format: Format) -> String {
    let stem: String = stem.chars().map(|c| if matches!(c, '/' | ':') { '-' } else { c }).collect();
    let stem = if stem.trim().is_empty() { "results".to_string() } else { stem };
    format!("{stem}.{}", format.extension())
}

/// How long a statement in an export may go unanswered: a COPY says
/// nothing until it has written the whole file.
const PATIENCE: Duration = Duration::from_secs(24 * 60 * 60);

/// A failure as the status line says it: the engine's own message when the
/// engine refused, without Harbor's code before it.
fn said(failure: harbor_client::Failure) -> String {
    match failure {
        harbor_client::Failure::Refused { message, .. } => message,
        other => other.to_string(),
    }
}

/// The engine's `xlsx` format comes from its `excel` extension, which is
/// neither installed nor loaded by default and does not load itself for a
/// COPY (measured). Loaded once, it serves every connection.
fn ensure_excel(conn: &Conn) -> Result<(), String> {
    let run = |sql: &str| harbor_client::exec_within(conn, sql, None, None, PATIENCE).map_err(said);
    if run("LOAD excel").is_ok() {
        return Ok(());
    }
    run("INSTALL excel").map_err(|e| format!("Excel export needs DuckDB's excel extension, which did not install: {e}"))?;
    run("LOAD excel").map_err(|e| format!("Excel export needs DuckDB's excel extension, which did not load: {e}"))?;
    Ok(())
}

/// Write the file, blocking: the COPY to the hidden name, then the rename
/// into place. On any failure nothing is left at either name. Returns the
/// rows written.
fn write(conn: &Conn, select: &str, format: Format, path: &Path, temp: &Path) -> Result<u64, String> {
    let (Some(temp_text), Some(_)) = (temp.to_str(), path.to_str()) else {
        return Err("the file's path is not UTF-8, which SQL cannot name".to_string());
    };
    if format == Format::Excel {
        ensure_excel(conn)?;
    }
    let sql = copy_sql(select, temp_text, copy_options(format, path));
    let outcome = harbor_client::exec_within(conn, &sql, None, None, PATIENCE)
        .map_err(said)
        .and_then(|result| {
            std::fs::rename(temp, path).map_err(|e| format!("the file was written but not moved into place: {e}"))?;
            Ok(crate::sql::count_of(&result).unwrap_or(0))
        });
    if outcome.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    outcome
}

/// The export under way, if any: one at a time.
struct Running {
    seq: u64,
    name: String,
    started: Instant,
    temp: PathBuf,
}

#[derive(Default)]
struct Exports {
    running: Option<Running>,
    seq: u64,
    /// The folder the last export went to, where the next one's panel
    /// opens.
    folder: Option<PathBuf>,
}

impl Global for Exports {}

/// Whether an export is running: a quit cuts it off (`QuitRisks`).
pub(crate) fn running(cx: &App) -> bool {
    cx.try_global::<Exports>().is_some_and(|e| e.running.is_some())
}

/// The app is ending: remove the hidden file of an export it cuts off.
/// The server stops a COPY whose client has gone, and removes what it wrote
/// except for Excel, so this is for that one, and for a quit that comes
/// before the server notices.
pub(crate) fn abandon(cx: &App) {
    if let Some(running) = cx.try_global::<Exports>().and_then(|e| e.running.as_ref()) {
        let _ = std::fs::remove_file(&running.temp);
    }
}

/// Ask where, then export. `plan` is the grid's (`Grid::export_plan`).
pub(crate) fn start(plan: Plan, chosen: Format, cx: &mut App) {
    if running(cx) {
        say(cx, "an export is already running", true, Some(Duration::from_secs(4)));
        return;
    }
    let folder = cx
        .try_global::<Exports>()
        .and_then(|e| e.folder.clone())
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join("Downloads")))
        .filter(|dir| dir.is_dir())
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/"));
    let rx = cx.prompt_for_new_path(&folder, Some(&suggested_name(&plan.stem, chosen)));
    cx.spawn(async move |cx| {
        if let Ok(Ok(Some(path))) = rx.await {
            cx.update(|cx| run(plan, chosen, path, cx));
        }
    })
    .detach();
}

fn run(plan: Plan, chosen: Format, path: PathBuf, cx: &mut App) {
    if running(cx) {
        say(cx, "an export is already running", true, Some(Duration::from_secs(4)));
        return;
    }
    let format = format_for(&path, chosen);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let exports = cx.default_global::<Exports>();
    exports.seq += 1;
    let seq = exports.seq;
    let temp = temp_path(&path, std::process::id(), seq);
    exports.folder = path.parent().map(Path::to_path_buf);
    exports.running = Some(Running { seq, name: name.clone(), started: Instant::now(), temp: temp.clone() });
    let mut said = say(cx, format!("exporting {name}\u{2026}"), false, None);

    let (done, verdict) = async_channel::bounded(1);
    let worker = std::thread::Builder::new().name("ducktable-export".into()).spawn(move || {
        let _ = done.send_blocking(write(&plan.conn, &plan.select, format, &path, &temp));
    });
    if let Err(e) = worker {
        finish(seq, Err(format!("could not start the export: {e}")), cx);
        return;
    }
    // The elapsed time ticks while the engine writes, once a copy's word
    // said meanwhile has had its time.
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            let still = cx.update(|cx| {
                let running = cx.try_global::<Exports>().and_then(|e| e.running.as_ref()).filter(|r| r.seq == seq)?;
                let text = format!(
                    "exporting {}\u{2026} {}",
                    running.name,
                    crate::util::human(running.started.elapsed().as_secs_f64(), "s")
                );
                let other = crate::footer::says_another(cx, said);
                if !other {
                    said = say(cx, text, false, None);
                }
                Some(())
            });
            if still.is_none() {
                break;
            }
        }
    })
    .detach();
    cx.spawn(async move |cx| {
        let outcome = verdict.recv().await.unwrap_or_else(|_| Err("the export stopped without an answer".to_string()));
        cx.update(|cx| finish(seq, outcome, cx));
    })
    .detach();
}

fn finish(seq: u64, outcome: Result<u64, String>, cx: &mut App) {
    let exports = cx.default_global::<Exports>();
    let Some(running) = exports.running.take_if(|r| r.seq == seq) else { return };
    let took = crate::util::human(running.started.elapsed().as_secs_f64(), "s");
    match outcome {
        Ok(rows) => say(
            cx,
            format!(
                "exported {} {} to {} \u{00b7} {took}",
                crate::util::commas(rows),
                if rows == 1 { "row" } else { "rows" },
                running.name
            ),
            false,
            Some(Duration::from_secs(8)),
        ),
        Err(why) => say(
            cx,
            format!("{} was not exported: {}", running.name, why.lines().next().unwrap_or_default()),
            true,
            Some(Duration::from_secs(20)),
        ),
    };
}

#[cfg(test)]
mod tests {
    use super::{copy_options, copy_sql, format_for, refusal, suggested_name, table_select, temp_path, Format};
    use std::path::Path;

    #[test]
    fn an_export_reads_the_rows_the_grid_pages() {
        use crate::sql::SortKey;
        let key = |column: &str, desc: bool| SortKey { column: column.to_string(), desc };
        let ties = vec!["id".to_string()];
        // The grid's page query is the export's SELECT with a LIMIT on it,
        // for every shape of filter under every shape of header sort: when
        // paging learns to read rows in another order, this fails until the
        // export reads them that way.
        for sort in [vec![], vec![key("name", true)], vec![key("x", false), key("id", true)]] {
            for filter in [
                None,
                Some("x > 0".to_string()),
                Some("x > 0 ORDER BY name DESC".to_string()),
                Some("ORDER BY name -- by name".to_string()),
                Some("id > 0 -- only positive".to_string()),
                Some("   ".to_string()),
            ] {
                let page = crate::sql::sorted_page_sql("\"main\".\"t\"", false, &filter, &sort, &ties, 0, 500);
                let select = table_select("\"main\".\"t\"", &filter, &sort, &ties);
                assert_eq!(page, format!("{select} LIMIT 500 OFFSET 0"), "{filter:?} {sort:?}");
            }
        }
        // The header sort, its tie-break after it, sets the strip's ORDER BY
        // aside, as on screen; with no header sort the strip's orders, as
        // typed.
        let filter = Some("x > 0 ORDER BY name".to_string());
        assert_eq!(
            table_select("t", &filter, &[key("v", true)], &ties),
            "SELECT * FROM t WHERE (\nx > 0\n)\nORDER BY \"v\" DESC NULLS LAST, \"id\" ASC\n"
        );
        assert_eq!(table_select("t", &filter, &[], &ties), "SELECT * FROM t WHERE (\nx > 0\n)\nORDER BY name\n");
        // A result is its statement, read as the grid reads it.
        let source = crate::sql::query_source("from t -- all of it");
        assert_eq!(table_select(&source, &None, &[], &[]), "SELECT * FROM (\nfrom t -- all of it\n)");
    }

    #[test]
    fn the_copy_quotes_its_path_and_keeps_the_select_on_its_own_lines() {
        let sql = copy_sql("SELECT * FROM t -- c", "/Users/x/it's \"a\" {b,c}\\d.csv", "FORMAT csv, HEADER true");
        assert_eq!(
            sql,
            "COPY (\nSELECT * FROM t -- c\n) TO '/Users/x/it''s \"a\" {b,c}\\d.csv' (FORMAT csv, HEADER true)"
        );
    }

    #[test]
    fn a_known_extension_names_the_format() {
        let p = |s: &str| Path::new(s).to_path_buf();
        assert_eq!(format_for(&p("/x/orders.csv"), Format::Excel), Format::Csv);
        assert_eq!(format_for(&p("/x/orders.PARQUET"), Format::Csv), Format::Parquet);
        assert_eq!(format_for(&p("/x/orders.csv.gz"), Format::Json), Format::Csv);
        assert_eq!(format_for(&p("/x/orders.jsonl"), Format::Csv), Format::Json);
        assert_eq!(format_for(&p("/x/orders.xlsx"), Format::Csv), Format::Excel);
        // No extension, or one of no format, keeps the menu's choice.
        assert_eq!(format_for(&p("/x/orders"), Format::Parquet), Format::Parquet);
        assert_eq!(format_for(&p("/x/orders.txt"), Format::Excel), Format::Excel);
        assert_eq!(copy_options(Format::Json, &p("/x/a.json")), "FORMAT json, ARRAY true");
        assert_eq!(copy_options(Format::Json, &p("/x/a.ndjson.gz")), "FORMAT json");
        assert_eq!(copy_options(Format::Csv, &p("/x/a.csv")), "FORMAT csv, HEADER true");
        assert_eq!(copy_options(Format::Excel, &p("/x/a.xlsx")), "FORMAT xlsx, HEADER true");
        assert_eq!(copy_options(Format::Parquet, &p("/x/a")), "FORMAT parquet");
    }

    #[test]
    fn the_hidden_name_sits_beside_the_file_and_ends_with_its_name() {
        let temp = temp_path(Path::new("/x/y/orders.csv.gz"), 42, 7);
        assert_eq!(temp, Path::new("/x/y/.ducktable-export-42-7-orders.csv.gz"));
        assert_eq!(suggested_name("sales.orders", Format::Parquet), "sales.orders.parquet");
        assert_eq!(suggested_name("a/b:c", Format::Excel), "a-b-c.xlsx");
        assert_eq!(suggested_name("", Format::Csv), "results.csv");
    }

    #[test]
    fn only_what_this_mac_can_write_is_exported() {
        assert_eq!(refusal(true, false, true, false), None);
        assert_eq!(refusal(true, true, true, false), None);
        assert!(refusal(false, false, true, false).unwrap().contains("another machine"));
        assert!(refusal(true, true, false, false).unwrap().contains("not a query"));
        assert!(refusal(true, true, true, true).unwrap().contains("transaction"));
        // A table's grid is exported whatever its paging says.
        assert_eq!(refusal(true, false, false, true), None);
    }

    /// Each format writes a file the engine reads back whole, under the
    /// grid's filter and ordering; a failed export leaves nothing at either
    /// name and an existing file as it was. Live: needs `HARBOR_LIVE_DB`, a
    /// scratch database file, as the probes in harbor-client's
    /// `tests/live.rs` do. Excel installs DuckDB's excel extension when it
    /// is missing, which needs the network once.
    #[test]
    #[ignore]
    fn every_format_writes_a_file_the_engine_reads_back() {
        use super::write;
        let Some(db) = std::env::var_os("HARBOR_LIVE_DB") else {
            println!("set HARBOR_LIVE_DB to a scratch database file; skipping");
            return;
        };
        let conn = harbor_client::fleet::connect_path(Path::new(&db)).expect("connect");
        let run = |sql: &str| harbor_client::query(&conn, sql).unwrap_or_else(|e| panic!("{e}\n{sql}"));
        let one = |sql: &str| run(sql).rows.remove(0);
        run("CREATE OR REPLACE TABLE _dt_export AS SELECT i::INTEGER i, \
             CASE WHEN i % 7 = 0 THEN NULL ELSE 'row ' || i || ', \"q\"' || chr(10) || 'it''s' END s, \
             DATE '2024-01-01' + i::INTEGER d, i / 4 x, (i % 3)::INTEGER g FROM range(1000) r(i)");
        let dir = std::env::temp_dir().join(format!("ducktable-export-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = "\"main\".\"_dt_export\"";
        let filter = Some("i % 2 = 0 ORDER BY i DESC".to_string());
        let select = table_select(source, &filter, &[], &[]);
        let want = one("SELECT count(*), sum(i), count(s), max(d)::VARCHAR FROM _dt_export WHERE i % 2 = 0");
        assert_eq!(want[0], 500);

        for (name, format, reader) in [
            ("it's \"q\" {a,b}.csv", Format::Csv, "read_csv"),
            ("orders.csv.gz", Format::Csv, "read_csv"),
            ("orders.parquet", Format::Parquet, "read_parquet"),
            ("orders.json", Format::Json, "read_json"),
            ("orders.ndjson", Format::Json, "read_json"),
            ("orders.xlsx", Format::Excel, "read_xlsx"),
        ] {
            let path = dir.join(name);
            let temp = temp_path(&path, 1, 1);
            let rows = write(&conn, &select, format_for(&path, format), &path, &temp).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(rows, 500, "{name}");
            assert!(path.is_file() && !temp.exists(), "{name}");
            let file = crate::util::qlit(path.to_str().unwrap());
            let got = one(&format!("SELECT count(*), sum(i::INTEGER), count(s), max(d::DATE)::VARCHAR FROM {reader}({file})"));
            assert_eq!(got, want, "{name}");
            // The rows are the grid's, in its order, the strings whole.
            let first = one(&format!("SELECT i::INTEGER, s FROM {reader}({file}) LIMIT 1"));
            assert_eq!(first, one("SELECT i, s FROM _dt_export WHERE i = 998"), "{name}");
            let lost = one(&format!(
                "SELECT count(*) FROM (SELECT i, s FROM _dt_export WHERE i % 2 = 0 \
                 EXCEPT SELECT i::INTEGER, s FROM {reader}({file}))"
            ));
            assert_eq!(lost[0], 0, "{name}");
        }

        // Under a header sort the file is in the order the grid's pages
        // show, the sort's ties broken as the pages break them (this keyless
        // table's rowid), the strip's ORDER BY set aside.
        let sort = [crate::sql::SortKey { column: "g".to_string(), desc: true }];
        let ties = ["rowid".to_string()];
        let sorted = table_select(source, &filter, &sort, &ties);
        let mut shown = Vec::new();
        for page in 0..4 {
            let sql = crate::sql::sorted_page_sql(source, true, &filter, &sort, &ties, page, 128);
            shown.extend(run(&sql).rows.into_iter().map(|r| r[1].clone()));
        }
        assert_eq!(shown.len(), 500);
        for (name, format, reader) in [
            ("sorted.csv", Format::Csv, "read_csv"),
            ("sorted.parquet", Format::Parquet, "read_parquet"),
            ("sorted.json", Format::Json, "read_json"),
            ("sorted.xlsx", Format::Excel, "read_xlsx"),
        ] {
            let path = dir.join(name);
            assert_eq!(write(&conn, &sorted, format, &path, &temp_path(&path, 1, 5)), Ok(500), "{name}");
            let file = crate::util::qlit(path.to_str().unwrap());
            let order: Vec<_> =
                run(&format!("SELECT i::INTEGER FROM {reader}({file})")).rows.into_iter().map(|r| r[0].clone()).collect();
            assert_eq!(order, shown, "{name}: the file's rows in the grid's order");
        }

        // A result exports its statement, a trailing comment and all.
        let path = dir.join("results.csv");
        let statement =
            table_select(&crate::sql::query_source("FROM _dt_export WHERE i < 10 -- the first ten"), &None, &[], &[]);
        assert_eq!(write(&conn, &statement, Format::Csv, &path, &temp_path(&path, 1, 2)), Ok(10));

        // A COPY that fails part way, in every format: the engine removes a
        // CSV, Parquet or JSON file but leaves an Excel one, so the hidden
        // name is removed here, and the file already there stays as it was.
        let failing = "SELECT CASE WHEN i > 30000 THEN error('boom') ELSE i END i FROM range(40000) r(i)";
        for (name, format) in [("f.csv", Format::Csv), ("f.parquet", Format::Parquet), ("f.json", Format::Json), ("f.xlsx", Format::Excel)] {
            let path = dir.join(name);
            std::fs::write(&path, "kept").unwrap();
            let temp = temp_path(&path, 1, 3);
            let err = write(&conn, failing, format, &path, &temp).expect_err(name);
            assert!(err.contains("boom"), "{name}: {err}");
            assert!(!temp.exists(), "{name}: the hidden file is left");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "kept", "{name}");
        }
        // A folder that is not there is the engine's to name.
        let nowhere = dir.join("missing").join("x.csv");
        let err = write(&conn, &select, Format::Csv, &nowhere, &temp_path(&nowhere, 1, 4)).expect_err("no folder");
        assert!(err.contains("No such file or directory"), "{err}");

        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name()).collect();
        assert!(left.iter().all(|n| !n.to_string_lossy().starts_with(".ducktable-export")), "{left:?}");
        std::fs::remove_dir_all(&dir).unwrap();
        run("DROP TABLE _dt_export");
    }
}
