//! `harbor <data file>`: a CSV, a folder of Parquet, a spreadsheet, a SQLite
//! file, opened in the workspace.
//!
//! What a path holds is `wire::open`'s question, answered the same way for
//! DuckTable. A DuckDB database opens as itself, as it always has; anything
//! else DuckDB reads opens in the workspace database
//! (`harbor_common::paths::workspace`), as a view named for the file or, for
//! SQLite, an attached catalog, made before the prompt and said in one line.
//! Opening the same file again finds what the first opening made. An
//! attachment is written to the workspace's record (`wire::open::Record`),
//! from which every server that opens the workspace attaches it again.

use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};

use harbor_http as http;
use http::Transport;
use serde_json::Value;
use wire::open::{self, Entry, Kind, Reader};
use wire::{Event, SqlRequest, endpoint};

/// Where a command-line target opens.
pub(crate) enum Opening {
    /// A database, a socket, a url or a name: resolved as it always is.
    AsIs,
    /// A file that opens in the workspace, by its absolute path.
    Workspace { file: PathBuf, kind: Kind },
}

/// What `target` holds, read from its name and first bytes. Only a path is
/// looked at: a url, a listed name and a socket are not files to open.
pub(crate) fn kind_of(target: &str) -> Result<Kind, String> {
    if super::url_server(target).is_some() || !harbor_common::looks_like_path(target) {
        return Ok(Kind::Duckdb);
    }
    let path = harbor_common::paths::expand(target);
    if super::is_socket(&path) || target.ends_with(".sock") {
        return Ok(Kind::Duckdb);
    }
    let mut head = Vec::with_capacity(open::HEAD_LEN);
    let entry = match std::fs::metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Entry::Missing,
        Err(e) => return Err(format!("{target}: {e}")),
        Ok(m) if m.is_dir() => Entry::Dir,
        Ok(_) => {
            let file = std::fs::File::open(&path).map_err(|e| format!("{target}: {e}"))?;
            file.take(open::HEAD_LEN as u64).read_to_end(&mut head).map_err(|e| format!("{target}: {e}"))?;
            Entry::File(&head)
        }
    };
    open::classify(target, entry)
}

/// Where `target` opens.
pub(crate) fn plan(target: &str) -> Result<Opening, String> {
    match kind_of(target)? {
        Kind::Duckdb => Ok(Opening::AsIs),
        kind => {
            // The server has its own working directory: the view and the
            // attachment name the file absolutely.
            let path = harbor_common::paths::expand(target);
            let file = std::path::absolute(&path).map_err(|e| format!("{target}: {e}"))?;
            Ok(Opening::Workspace { file, kind })
        }
    }
}

/// The workspace database, its directory made if it is not there.
pub(crate) fn workspace() -> Result<PathBuf, String> {
    let db = harbor_common::paths::workspace()?;
    if let Some(dir) = db.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    Ok(db)
}

/// Make the view or the attachment for `file` in the workspace `transport`
/// serves, or find the one an earlier opening made, and say which in one
/// line.
pub(crate) fn into_workspace(transport: &Transport, file: &Path, kind: Kind) -> Result<String, String> {
    let path = file.to_str().ok_or_else(|| format!("{}: not a UTF-8 path", file.display()))?;
    let shown = harbor_common::paths::shorten(file);
    match kind {
        Kind::Data(reader) => view(transport, path, &shown, reader),
        Kind::Sqlite => attach(transport, path, &shown),
        Kind::Duckdb => Err(format!("{shown} is a DuckDB database; it opens as itself")),
    }
}

fn view(transport: &Transport, path: &str, shown: &str, reader: Reader) -> Result<String, String> {
    // Loaded in every server that reads the view, since the engine does not
    // load it on demand; installed once, from DuckDB's repository.
    if let Some(extension) = reader.extension() {
        rows(transport, &format!("INSTALL {extension}"))?;
        rows(transport, &format!("LOAD {extension}"))?;
    }
    let what = format!("a view over {}{shown}", reader.describes());
    let existing = rows(
        transport,
        "SELECT view_name, sql FROM duckdb_views() \
         WHERE database_name = current_database() AND schema_name = 'main' \
           AND NOT internal AND NOT temporary ORDER BY view_name",
    )?;
    if let Some(row) = existing.iter().find(|row| open::reads(text(row, 1), reader, path)) {
        return Ok(format!("{}: {what}, already in the workspace", text(row, 0)));
    }
    let taken = names(
        transport,
        "SELECT table_name FROM duckdb_tables() WHERE database_name = current_database() AND schema_name = 'main' \
         UNION ALL SELECT view_name FROM duckdb_views() WHERE database_name = current_database() AND schema_name = 'main' \
         UNION ALL SELECT keyword_name FROM duckdb_keywords() WHERE keyword_category = 'reserved'",
    )?;
    let name = open::free_name(&open::view_name(path), |n| taken.iter().any(|t| t.eq_ignore_ascii_case(n)));
    rows(transport, &open::view_sql(&name, reader, path))?;
    Ok(format!("{name}: {what}"))
}

fn attach(transport: &Transport, path: &str, shown: &str) -> Result<String, String> {
    let existing = rows(
        transport,
        &format!(
            "SELECT database_name, readonly FROM duckdb_databases() WHERE path = {}",
            open::quote_literal(path)
        ),
    )?;
    if let Some(row) = existing.first() {
        remember(transport, text(row, 0), path, row.get(1).and_then(Value::as_bool).unwrap_or_default())?;
        return Ok(format!("{}: the SQLite database {shown}, attached already", text(row, 0)));
    }
    // A catalog's name must not be a database's, a schema's or a reserved
    // word, or `name.table` would mean something else; nor one the record
    // keeps for another file that its server could not attach, which would
    // be written over. The record is not there until a first attachment.
    let mut taken = names(
        transport,
        "SELECT database_name FROM duckdb_databases() \
         UNION ALL SELECT schema_name FROM duckdb_schemas() \
         UNION ALL SELECT keyword_name FROM duckdb_keywords() WHERE keyword_category = 'reserved'",
    )?;
    let recorded = format!(
        "SELECT name FROM {}.{} WHERE path <> {}",
        open::RECORD_SCHEMA,
        open::RECORD_TABLE,
        open::quote_literal(path)
    );
    taken.extend(names(transport, &recorded).unwrap_or_default());
    let name = open::free_name(&open::view_name(path), |n| taken.iter().any(|t| t.eq_ignore_ascii_case(n)));
    rows(transport, &open::attach_sql(&name, path))?;
    remember(transport, &name, path, false)?;
    Ok(format!("{name}: the SQLite database {shown}, attached"))
}

/// Record the attachment in the workspace, so every server that opens it
/// attaches the file again. The server rewrites the record from what is
/// attached when it stops; writing it here keeps it through a server that
/// does not stop cleanly.
fn remember(transport: &Transport, name: &str, path: &str, read_only: bool) -> Result<(), String> {
    for ddl in open::RECORD_DDL {
        rows(transport, ddl)?;
    }
    let record = open::Record { name: name.into(), path: path.into(), storage: "sqlite".into(), read_only };
    rows(transport, &record.insert_sql()).map(drop)
}

fn text(row: &[Value], i: usize) -> &str {
    row.get(i).and_then(Value::as_str).unwrap_or_default()
}

fn names(transport: &Transport, sql: &str) -> Result<Vec<String>, String> {
    Ok(rows(transport, sql)?.iter().map(|row| text(row, 0).to_string()).collect())
}

/// Run one statement and read its rows, or the engine's refusal.
fn rows(transport: &Transport, sql: &str) -> Result<Vec<Vec<Value>>, String> {
    let body = serde_json::to_string(&SqlRequest { sql: sql.into(), ..Default::default() })
        .map_err(|e| e.to_string())?;
    let response = http::request(transport, &endpoint::SQL, Some(&body), None).map_err(|e| e.to_string())?;
    if !(200..300).contains(&response.status) {
        let status = response.status;
        let text = response.body_string().map_err(|e| e.to_string())?;
        return Err(http::Failure::of(status, &text).to_string());
    }
    let mut out = Vec::new();
    for line in response.body.lines() {
        match Event::parse(line.map_err(|e| e.to_string())?.trim()) {
            Ok(Event::Row { values }) => out.push(values),
            Ok(Event::Error { code, message }) => return Err(format!("{code}: {message}")),
            _ => {}
        }
    }
    Ok(out)
}
