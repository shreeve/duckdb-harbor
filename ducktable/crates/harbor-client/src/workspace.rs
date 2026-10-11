//! The workspace: the DuckDB database a file that is not a DuckDB database
//! opens in, shared with `harbor <data file>` (`paths::workspace`).
//!
//! What a path holds is `wire::open`'s question, answered here as harbor's
//! command line answers it, so DuckTable and the CLI agree on every file.
//! A data file opens as a view that reads it where it lies, a SQLite file
//! as an attached catalog; opening a file a second time finds what the
//! first opening made. Everything here blocks, and runs on a pooled
//! connection: callers run it off the UI thread.

use crate::fleet::Conn;
use serde_json::Value;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;
use wire::catalog::Catalog;
use wire::open::{self, Entry, Kind, Reader, Record};

/// The workspace database, its directory made when it is not there.
pub fn path() -> Result<PathBuf, String> {
    let db = harbor_common::paths::workspace()?;
    if let Some(dir) = db.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    Ok(db)
}

/// What `path` holds, from its name and its first bytes.
pub fn kind_of(path: &Path) -> Result<Kind, String> {
    let shown = path.display().to_string();
    let mut head = Vec::with_capacity(open::HEAD_LEN);
    let entry = match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Entry::Missing,
        Err(e) => return Err(format!("{shown}: {e}")),
        Ok(m) if m.is_dir() => Entry::Dir,
        Ok(_) => {
            let file = std::fs::File::open(path).map_err(|e| format!("{shown}: {e}"))?;
            file.take(open::HEAD_LEN as u64).read_to_end(&mut head).map_err(|e| format!("{shown}: {e}"))?;
            Entry::File(&head)
        }
    };
    open::classify(&shown, entry)
}

/// What opening a file in the workspace made or found.
#[derive(Debug, Clone, PartialEq)]
pub struct Opened {
    /// The view's name, or the attached catalog's.
    pub name: String,
    /// True for a SQLite file's catalog, false for a data file's view.
    pub attached: bool,
    /// Made by this opening, or found from an earlier one.
    pub made: bool,
    /// One line for people: `sales: a view over ~/x/sales.csv`.
    pub line: String,
}

/// Make the view or the attachment for `file` in the workspace `conn`
/// serves, or find the one an earlier opening made.
pub fn open(conn: &Conn, file: &Path, kind: Kind) -> Result<Opened, String> {
    let file = std::path::absolute(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let path = file.to_str().ok_or_else(|| format!("{}: not a UTF-8 path, which SQL cannot name", file.display()))?;
    let shown = harbor_common::paths::shorten(&file);
    match kind {
        Kind::Data(reader) => view(conn, path, &shown, reader),
        Kind::Sqlite => attach(conn, path, &shown),
        Kind::Duckdb => Err(format!("{shown} is a DuckDB database; it opens as itself")),
    }
}

fn view(conn: &Conn, path: &str, shown: &str, reader: Reader) -> Result<Opened, String> {
    if let Some(extension) = reader.extension() {
        load_extension(conn, extension)?;
    }
    let what = format!("a view over {}{shown}", reader.describes());
    let existing = rows(
        conn,
        "SELECT view_name, sql FROM duckdb_views() \
         WHERE database_name = current_database() AND schema_name = 'main' \
           AND NOT internal AND NOT temporary ORDER BY view_name",
    )?;
    if let Some(row) = existing.iter().find(|row| open::reads(text(row, 1), reader, path)) {
        let name = text(row, 0).to_string();
        let line = format!("{name}: {what}, already in the workspace");
        return Ok(Opened { name, attached: false, made: false, line });
    }
    let taken = names(
        conn,
        "SELECT table_name FROM duckdb_tables() WHERE database_name = current_database() AND schema_name = 'main' \
         UNION ALL SELECT view_name FROM duckdb_views() WHERE database_name = current_database() AND schema_name = 'main' \
         UNION ALL SELECT keyword_name FROM duckdb_keywords() WHERE keyword_category = 'reserved'",
    )?;
    let name = open::free_name(&open::view_name(path), |n| taken.iter().any(|t| t.eq_ignore_ascii_case(n)));
    rows(conn, &open::view_sql(&name, reader, path))?;
    let line = format!("{name}: {what}");
    Ok(Opened { name, attached: false, made: true, line })
}

fn attach(conn: &Conn, path: &str, shown: &str) -> Result<Opened, String> {
    let existing = rows(
        conn,
        &format!("SELECT database_name, readonly FROM duckdb_databases() WHERE path = {}", open::quote_literal(path)),
    )?;
    if let Some(row) = existing.first() {
        let name = text(row, 0).to_string();
        record_attachment(conn, &name, path, row.get(1).and_then(Value::as_bool).unwrap_or_default())?;
        let line = format!("{name}: the SQLite database {shown}, attached already");
        return Ok(Opened { name, attached: true, made: false, line });
    }
    // A catalog's name must not be a database's, a schema's or a reserved
    // word, or `name.table` would mean something else; nor one the record
    // keeps for another file that its server could not attach, whose row
    // it would write over. The record is not there until a first
    // attachment.
    let mut taken = names(
        conn,
        "SELECT database_name FROM duckdb_databases() \
         UNION ALL SELECT schema_name FROM duckdb_schemas() \
         UNION ALL SELECT keyword_name FROM duckdb_keywords() WHERE keyword_category = 'reserved'",
    )?;
    taken.extend(names(conn, &Record::names_sql(path)).unwrap_or_default());
    let name = open::free_name(&open::view_name(path), |n| taken.iter().any(|t| t.eq_ignore_ascii_case(n)));
    rows(conn, &open::attach_sql(&name, path))?;
    record_attachment(conn, &name, path, false)?;
    let line = format!("{name}: the SQLite database {shown}, attached");
    Ok(Opened { name, attached: true, made: true, line })
}

/// Record the attachment in the workspace (`wire::open::Record`), so every
/// server that opens it attaches the file again. The server rewrites the
/// record from what is attached when it stops; writing it here, as the
/// file is attached, keeps it through a server that does not stop cleanly.
fn record_attachment(conn: &Conn, name: &str, path: &str, read_only: bool) -> Result<(), String> {
    for ddl in open::RECORD_DDL {
        rows(conn, ddl)?;
    }
    let record = Record { name: name.into(), path: path.into(), storage: "sqlite".into(), read_only };
    rows(conn, &record.insert_sql()).map(drop)
}

/// The statements Detach runs for the catalog `name`: `DETACH` when it is
/// `attached`, and, in the `workspace`, its row deleted from the record, so
/// no later server attaches it again. A catalog the workspace records and
/// could not attach (its file gone) has nothing to detach, only its row.
pub fn detach_statements(name: &str, attached: bool, workspace: bool) -> Vec<String> {
    let mut sql = Vec::new();
    if attached {
        sql.push(format!("DETACH {}", open::quote_ident(name)));
    }
    if workspace {
        sql.extend(open::RECORD_DDL.iter().map(|s| s.to_string()));
        sql.push(Record::delete_sql(name));
    }
    sql
}

/// Detach the catalog `name` ([`detach_statements`]). The file is
/// untouched.
pub fn detach(conn: &Conn, name: &str, attached: bool, workspace: bool) -> Result<(), String> {
    for sql in detach_statements(name, attached, workspace) {
        rows(conn, &sql)?;
    }
    Ok(())
}

/// Drop the view `name` in schema `schema`. The file it reads is untouched.
pub fn remove_view(conn: &Conn, schema: &str, name: &str) -> Result<(), String> {
    rows(conn, &format!("DROP VIEW {}.{}", open::quote_ident(schema), open::quote_ident(name)))?;
    Ok(())
}

/// The extensions that views of `catalog` which do not read are missing:
/// `spatial`, which the engine does not load on demand, for a view over
/// `ST_Read`. A server that has not loaded it cannot read such a view at
/// all, so it is loaded and the catalog read again for the sidebar.
pub fn needed_extensions(catalog: &Catalog) -> Vec<&'static str> {
    let unread_spatial = |v: &&wire::catalog::View| {
        v.error.is_some() && v.ddl.as_deref().is_some_and(|d| d.to_lowercase().contains("st_read("))
    };
    let spatial = catalog.views.iter().chain(catalog.attached.iter().flat_map(|a| a.views.iter())).any(|v| unread_spatial(&v));
    match spatial {
        true => vec!["spatial"],
        false => Vec::new(),
    }
}

/// Load `extension` in the server, installing it first, once, from
/// DuckDB's repository when it is not there. Loaded once, it serves every
/// connection the server holds.
pub fn load_extension(conn: &Conn, extension: &str) -> Result<(), String> {
    if rows(conn, &format!("LOAD {extension}")).is_ok() {
        return Ok(());
    }
    rows(conn, &format!("INSTALL {extension}"))
        .map_err(|e| format!("the {extension} extension did not install: {e}"))?;
    rows(conn, &format!("LOAD {extension}")).map_err(|e| format!("the {extension} extension did not load: {e}"))?;
    Ok(())
}

/// The file or folder a view of the workspace reads, from its SQL as the
/// engine renders it (`read_csv('/x/sales.csv')`): the first string
/// literal, its quotes undoubled and its glob brackets undone, and for a
/// folder of Parquet the folder. None when the view reads no file.
pub fn view_source(ddl: &str) -> Option<PathBuf> {
    let lower = ddl.to_lowercase();
    let call = ["read_csv(", "read_parquet(", "read_json(", "read_xlsx(", "st_read("]
        .iter()
        .filter_map(|f| lower.find(f).map(|at| at + f.len()))
        .min()?;
    let rest = ddl[call..].trim_start().strip_prefix('\'')?;
    let mut literal = String::new();
    let mut chars = rest.chars().peekable();
    loop {
        match chars.next()? {
            '\'' if chars.peek() == Some(&'\'') => {
                chars.next();
                literal.push('\'');
            }
            '\'' => break,
            c => literal.push(c),
        }
    }
    let path = literal.replace("[*]", "*").replace("[?]", "?").replace("[[]", "[");
    let path = path.strip_suffix("/**/*.parquet").unwrap_or(&path);
    Some(PathBuf::from(path))
}

/// Bytes on disk at `path`: a file's length, or every file beneath a
/// folder. None when nothing is there.
pub fn size_on_disk(path: &Path) -> Option<u64> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_dir() {
        return Some(meta.len());
    }
    let mut total = 0;
    for entry in std::fs::read_dir(path).ok()?.flatten() {
        total += size_on_disk(&entry.path()).unwrap_or(0);
    }
    Some(total)
}

/// The statements Import as Table runs, in one transaction: the view's
/// rows copied into a table under a free name, the view dropped, and the
/// table renamed to the view's name, so the name never means nothing and a
/// failure leaves the view as it was.
pub fn import_statements(schema: &str, view: &str, scratch: &str) -> Vec<String> {
    let (s, v, t) = (open::quote_ident(schema), open::quote_ident(view), open::quote_ident(scratch));
    vec![
        "BEGIN".to_string(),
        format!("CREATE TABLE {s}.{t} AS SELECT * FROM {s}.{v}"),
        format!("DROP VIEW {s}.{v}"),
        format!("ALTER TABLE {s}.{t} RENAME TO {v}"),
        "COMMIT".to_string(),
    ]
}

/// Import as Table: copy the view `name` into a table of the same name, on
/// a session of its own, and drop the view. The copy reads the whole file,
/// so it is given as long as it takes.
pub fn import(conn: &Conn, schema: &str, name: &str) -> Result<u64, String> {
    let taken = names(
        conn,
        &format!(
            "SELECT table_name FROM duckdb_tables() WHERE database_name = current_database() AND schema_name = {s} \
             UNION ALL SELECT view_name FROM duckdb_views() WHERE database_name = current_database() AND schema_name = {s}",
            s = open::quote_literal(schema)
        ),
    )?;
    let scratch = open::free_name(&format!("{name}_import"), |n| taken.iter().any(|t| t.eq_ignore_ascii_case(n)));
    let session = crate::session_new(conn)?;
    let patience = Duration::from_secs(24 * 60 * 60);
    let mut copied = 0;
    let mut outcome = Ok(());
    for sql in import_statements(schema, name, &scratch) {
        match crate::exec_within(conn, &sql, None, Some(&session), patience) {
            Ok(result) if sql.starts_with("CREATE") => {
                copied = result.rows.first().and_then(|r| r.first()).and_then(Value::as_u64).unwrap_or(0);
            }
            Ok(_) => {}
            Err(e) => {
                outcome = Err(e.to_string());
                break;
            }
        }
    }
    if outcome.is_err() {
        let _ = crate::exec(conn, "ROLLBACK", None, Some(&session));
    }
    crate::session_release(conn, &session);
    outcome.map(|()| copied)
}

fn text(row: &[Value], i: usize) -> &str {
    row.get(i).and_then(Value::as_str).unwrap_or_default()
}

fn names(conn: &Conn, sql: &str) -> Result<Vec<String>, String> {
    Ok(rows(conn, sql)?.iter().map(|row| text(row, 0).to_string()).collect())
}

/// One statement on a pooled connection, its rows or the engine's refusal.
/// An `INSTALL` fetches from the network and a view over a large file binds
/// slowly, so each is given two minutes.
fn rows(conn: &Conn, sql: &str) -> Result<Vec<Vec<Value>>, String> {
    crate::exec_within(conn, sql, None, None, Duration::from_secs(120))
        .map(|r| r.rows)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wire::catalog::{Attached, View};

    #[test]
    fn a_views_source_is_read_back_from_the_engines_sql() {
        let csv = "CREATE VIEW sales AS SELECT * FROM read_csv('/x/it''s [[]1][*].csv');";
        assert_eq!(view_source(csv), Some(PathBuf::from("/x/it's [1]*.csv")));
        let lake = "CREATE VIEW lake AS SELECT * FROM read_parquet('/l/**/*.parquet', (hive_partitioning = true));";
        assert_eq!(view_source(lake), Some(PathBuf::from("/l")));
        let shp = "CREATE VIEW pts AS SELECT * FROM st_read('/p [x].shp');";
        assert_eq!(view_source(shp), Some(PathBuf::from("/p [x].shp")));
        assert_eq!(view_source("CREATE VIEW v AS SELECT 42;"), None);
        assert_eq!(view_source("CREATE VIEW v AS SELECT * FROM read_csv(x);"), None);
    }

    #[test]
    fn a_spatial_view_that_does_not_read_asks_for_the_spatial_extension() {
        let unread = Some("Catalog Error: Table Function with name st_read does not exist!".to_string());
        let view = |ddl: &str, error: &Option<String>| View {
            name: "v".into(),
            schema: "main".into(),
            ddl: Some(ddl.into()),
            error: error.clone(),
            ..Default::default()
        };
        let csv = view("CREATE VIEW v AS SELECT * FROM read_csv('/gone.csv');", &Some("IO Error: No files found".into()));
        let mut catalog = Catalog { views: vec![csv], ..Default::default() };
        assert!(needed_extensions(&catalog).is_empty());
        // A spatial view that reads needs nothing more.
        catalog.views.push(view("CREATE VIEW p AS SELECT * FROM ST_Read('/p.geojson');", &None));
        assert!(needed_extensions(&catalog).is_empty());
        catalog.views.push(view("CREATE VIEW q AS SELECT * FROM ST_Read('/q.geojson');", &unread));
        assert_eq!(needed_extensions(&catalog), ["spatial"]);
        let attached = Attached { views: vec![view("CREATE VIEW p AS SELECT * FROM st_read('/p.shp');", &unread)], ..Default::default() };
        let catalog = Catalog { attached: vec![attached], ..Default::default() };
        assert_eq!(needed_extensions(&catalog), ["spatial"]);
    }

    #[test]
    fn detach_lets_go_of_the_catalog_and_its_record() {
        let ddl = open::RECORD_DDL.map(String::from);
        let forget = "DELETE FROM _harbor.attached WHERE name = 'o''x'".to_string();
        let attached = detach_statements("o'x", true, true);
        assert_eq!(attached[0], "DETACH \"o'x\"");
        assert_eq!(attached[1..], [ddl[0].clone(), ddl[1].clone(), forget.clone()]);
        // Recorded and not attached: only its row.
        assert_eq!(detach_statements("o'x", false, true), [ddl[0].clone(), ddl[1].clone(), forget]);
        // Another database keeps no record.
        assert_eq!(detach_statements("o'x", true, false), ["DETACH \"o'x\""]);
    }

    #[test]
    fn import_copies_drops_and_renames_in_one_transaction() {
        assert_eq!(
            import_statements("main", "it's \"q\"", "q_import"),
            [
                "BEGIN",
                "CREATE TABLE \"main\".\"q_import\" AS SELECT * FROM \"main\".\"it's \"\"q\"\"\"",
                "DROP VIEW \"main\".\"it's \"\"q\"\"\"",
                "ALTER TABLE \"main\".\"q_import\" RENAME TO \"it's \"\"q\"\"\"",
                "COMMIT",
            ]
        );
    }

    #[test]
    fn a_folder_is_as_big_as_the_files_beneath_it() {
        let dir = std::env::temp_dir().join(format!("dt-size-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("year=2024")).unwrap();
        std::fs::write(dir.join("a.parquet"), [0u8; 10]).unwrap();
        std::fs::write(dir.join("year=2024").join("b.parquet"), [0u8; 32]).unwrap();
        assert_eq!(size_on_disk(&dir), Some(42));
        assert_eq!(size_on_disk(&dir.join("a.parquet")), Some(10));
        assert_eq!(size_on_disk(&dir.join("gone")), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_path_is_classified_by_its_name_and_first_bytes() {
        let dir = std::env::temp_dir().join(format!("dt-kind-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("s.csv"), "a,b\n1,2\n").unwrap();
        std::fs::write(dir.join("app.db"), b"SQLite format 3\0and more").unwrap();
        std::fs::write(dir.join("x.parquet"), "a,b\n").unwrap();
        assert_eq!(kind_of(&dir.join("s.csv")), Ok(Kind::Data(Reader::Csv)));
        assert_eq!(kind_of(&dir.join("app.db")), Ok(Kind::Sqlite));
        assert_eq!(kind_of(&dir), Ok(Kind::Data(Reader::ParquetFolder)));
        assert!(kind_of(&dir.join("x.parquet")).unwrap_err().contains("not Parquet"));
        assert!(kind_of(&dir.join("gone.csv")).unwrap_err().contains("no such file"));
        assert_eq!(kind_of(&dir.join("new.duckdb")), Ok(Kind::Duckdb));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
