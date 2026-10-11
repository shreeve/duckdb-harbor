//! The workspace, made whole at every start.
//!
//! The workspace database (`harbor_common::paths::workspace`) holds views
//! over files and the record of database files attached in it
//! (`wire::open::Record`). A view lasts in the database's own catalog; an
//! `ATTACH` lasts only as long as the server that ran it, and a view that
//! reads with `ST_Read` reads only in a server that has loaded `spatial`,
//! which the engine does not load on demand. So a server that opens the
//! workspace, before it serves anyone, loads the extensions its views call
//! into and attaches each recorded file again ([`open`]), and when it stops
//! it rewrites the record from what is attached then ([`record`]), so an
//! `ATTACH` or `DETACH` from any client lasts as the client meant it.
//!
//! A recorded file that is gone, that the engine will not attach, or whose
//! catalog does not read once attached (not SQLite after all, or locked by
//! another program), is said in the server's log and in `/catalog`, as an
//! attached database with its reason in `error` ([`unattached`]); the
//! server starts all the same, and the row stays, so the file is attached
//! again at a later start once it reads. A row deleted from the record is
//! forgotten.

use std::sync::Mutex;

use wire::open::{self, Record, RECORD_DDL, RECORD_SCHEMA, RECORD_TABLE};

use crate::engine::conn::Conn;

/// A recorded file the server did not attach, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unattached {
    pub record: Record,
    pub reason: String,
}

/// What a server does with the workspace before it serves.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// `LOAD`, each once, in the order the views first call into them.
    pub loads: Vec<&'static str>,
    /// `ATTACH`, in the record's order.
    pub attaches: Vec<Record>,
    /// Recorded, and not attached, with why.
    pub unattached: Vec<Unattached>,
}

/// The plan for a workspace whose views read as `views` (each view's SQL)
/// and whose record holds `records`, in a server where `attached` are
/// attached already (by `--init`, say) and `exists` says whether a file is
/// there. A record whose name is attached already is passed over; one whose
/// file is gone is not attached, since attaching a missing path makes an
/// empty database there; one that is neither a SQLite nor a DuckDB file is
/// refused, as the server never records one.
pub fn plan(views: &[String], records: &[Record], attached: &[String], exists: impl Fn(&str) -> bool) -> Plan {
    let mut plan = Plan::default();
    for sql in views {
        for extension in open::extensions(sql) {
            if !plan.loads.contains(&extension) {
                plan.loads.push(extension);
            }
        }
    }
    for record in records {
        if attached.iter().any(|name| name.eq_ignore_ascii_case(&record.name)) {
            continue;
        }
        let reason = match () {
            _ if !Record::kept(&record.storage, &record.path) => {
                format!("{}: a {} database, which the workspace does not attach", record.path, record.storage)
            }
            _ if !exists(&record.path) => format!("{}: no such file", record.path),
            _ => {
                plan.attaches.push(record.clone());
                continue;
            }
        };
        plan.unattached.push(Unattached { record: record.clone(), reason });
    }
    plan
}

/// The statements that rewrite the record when the server stops: a row for
/// each of `current`, the databases attached then, and no row for anything
/// else but `kept`, the names the server could not attach when it started,
/// whose rows stay as they are. In one transaction, after the record is
/// made where it is not; nothing when nothing is attached and the record
/// does not exist.
pub fn rewrite(current: &[Record], kept: &[String], exists: bool) -> Option<String> {
    if current.is_empty() && !exists {
        return None;
    }
    let mut keep: Vec<String> = current.iter().map(|r| open::quote_literal(&r.name)).collect();
    keep.extend(kept.iter().map(|name| open::quote_literal(name)));
    let mut sql: Vec<String> = RECORD_DDL.iter().map(|s| s.to_string()).collect();
    sql.insert(0, "BEGIN".into());
    sql.push(match keep.is_empty() {
        true => format!("DELETE FROM {RECORD_SCHEMA}.{RECORD_TABLE}"),
        false => format!("DELETE FROM {RECORD_SCHEMA}.{RECORD_TABLE} WHERE name NOT IN ({})", keep.join(", ")),
    });
    sql.extend(current.iter().map(Record::insert_sql));
    sql.push("COMMIT".into());
    Some(sql.join(";\n"))
}

/// Set when this server serves the workspace: what it could not attach.
static SERVING: Mutex<Option<Vec<Unattached>>> = Mutex::new(None);

/// Whether this server serves the workspace.
pub fn serving() -> bool {
    SERVING.lock().unwrap().is_some()
}

/// What this server could not attach when it started, for `/catalog`.
pub fn unattached() -> Vec<Unattached> {
    SERVING.lock().unwrap().clone().unwrap_or_default()
}

/// Make the workspace whole on `con`, the connection the pool is cloned
/// from, before it serves: what is loaded and attached here is the whole
/// instance's. Said in the server's log; nothing here stops a start.
pub fn open(con: &mut Conn) {
    let views = con
        .query_strings(
            "SELECT sql FROM duckdb_views() \
             WHERE database_name = current_database() AND NOT internal AND NOT temporary",
        )
        .unwrap_or_else(|e| {
            eprintln!("harbor: workspace: its views could not be read: {e}");
            Vec::new()
        });
    let records = read_records(con);
    let attached = con
        .query_strings("SELECT database_name FROM duckdb_databases() WHERE NOT internal")
        .unwrap_or_default();
    let plan = plan(&views, &records, &attached, |path| std::path::Path::new(path).is_file());

    for extension in &plan.loads {
        // Installed by whoever made the view; installed here when it is not,
        // once, from DuckDB's repository.
        let loaded = con
            .execute_batch(&format!("LOAD {extension}"))
            .or_else(|_| con.execute_batch(&format!("INSTALL {extension}; LOAD {extension}")));
        if let Err(e) = loaded {
            eprintln!("harbor: workspace: {extension} did not load, and the views that need it say so: {e}");
        }
    }
    let mut unattached = plan.unattached;
    for record in plan.attaches {
        if let Err(reason) = attach(con, &record) {
            unattached.push(Unattached { record, reason });
        }
    }
    for u in &unattached {
        eprintln!("harbor: workspace: {} is not attached — {}", u.record.name, u.reason);
    }
    *SERVING.lock().unwrap() = Some(unattached);
}

/// Attach a recorded file again, and read its catalog. Attaching a SQLite
/// file reads nothing of it, so a file that is not SQLite, or one another
/// program holds locked, attaches; and then every catalog read of the
/// server fails, since the engine reads every attached database's catalog
/// for any of them. One whose catalog does not read is detached again, with
/// the engine's reason.
fn attach(con: &mut Conn, record: &Record) -> Result<(), String> {
    con.execute_batch(&record.attach_sql()).map_err(|e| e.to_string())?;
    let read = con.query_strings("SELECT count(*)::VARCHAR FROM duckdb_tables()");
    let Err(e) = read else { return Ok(()) };
    if let Err(detach) = con.execute_batch(&format!("DETACH {}", open::quote_ident(&record.name))) {
        eprintln!("harbor: workspace: {} could not be detached again: {detach}", record.name);
    }
    Err(e.to_string())
}

/// The record's rows, or none when it has not been made.
fn read_records(con: &mut Conn) -> Vec<Record> {
    if !record_exists(con) {
        return Vec::new();
    }
    let rows = con.query_strings(&format!(
        "SELECT json_object('name', name, 'path', path, 'type', type, 'read_only', read_only)::VARCHAR \
         FROM {RECORD_SCHEMA}.{RECORD_TABLE} ORDER BY name"
    ));
    match rows {
        Ok(rows) => rows.iter().filter_map(|row| serde_json::from_str(row).ok()).collect(),
        Err(e) => {
            eprintln!("harbor: workspace: its record of attached files could not be read: {e}");
            Vec::new()
        }
    }
}

fn record_exists(con: &mut Conn) -> bool {
    con.query_strings(&format!(
        "SELECT table_name FROM duckdb_tables() WHERE database_name = current_database() \
         AND schema_name = '{RECORD_SCHEMA}' AND table_name = '{RECORD_TABLE}'"
    ))
    .is_ok_and(|rows| !rows.is_empty())
}

/// Rewrite the record from what is attached on `con` as the server stops
/// ([`rewrite`]). A failure is said and leaves the record as it was.
pub fn record(con: &mut Conn) {
    let Some(unattached) = SERVING.lock().unwrap().clone() else { return };
    // An executor abandoned at the stop may still be attaching or detaching.
    let _reading = crate::reading_catalog();
    let current = con.query_strings(
        "SELECT json_object('name', database_name, 'path', path, 'type', type, 'read_only', readonly)::VARCHAR \
         FROM duckdb_databases() \
         WHERE NOT internal AND database_name <> current_database() AND path IS NOT NULL \
         ORDER BY database_name",
    );
    let current: Vec<Record> = match current {
        Ok(rows) => rows
            .iter()
            .filter_map(|row| serde_json::from_str::<Record>(row).ok())
            .filter(|r| Record::kept(&r.storage, &r.path))
            .collect(),
        Err(e) => {
            eprintln!("harbor: workspace: what is attached could not be read, and the record stays as it was: {e}");
            return;
        }
    };
    let kept: Vec<String> = unattached
        .into_iter()
        .map(|u| u.record.name)
        .filter(|name| !current.iter().any(|r| r.name.eq_ignore_ascii_case(name)))
        .collect();
    let exists = record_exists(con);
    let Some(sql) = rewrite(&current, &kept, exists) else { return };
    if let Err(e) = con.execute_batch(&sql) {
        let _ = con.execute_batch("ROLLBACK");
        eprintln!("harbor: workspace: the record of attached files could not be written, and stays as it was: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(name: &str, path: &str, storage: &str) -> Record {
        Record { name: name.into(), path: path.into(), storage: storage.into(), read_only: false }
    }

    #[test]
    fn views_that_call_st_functions_load_spatial_once() {
        let views = [
            "CREATE VIEW sales AS SELECT * FROM read_csv('/x/sales.csv');".to_string(),
            "CREATE VIEW parks AS SELECT * FROM st_read('/x/parks.geojson');".to_string(),
            "CREATE VIEW book AS SELECT * FROM read_xlsx('/x/book.xlsx');".to_string(),
            "CREATE VIEW roads AS SELECT * FROM st_read('/x/roads.shp');".to_string(),
        ];
        let p = plan(&views, &[], &[], |_| true);
        assert_eq!(p.loads, ["spatial"]);
        assert!(p.attaches.is_empty() && p.unattached.is_empty());
        // Excel, JSON and Parquet load on demand; nothing is loaded for them.
        assert!(plan(&views[..1], &[], &[], |_| true).loads.is_empty());
        assert!(plan(&[views[2].clone()], &[], &[], |_| true).loads.is_empty());
    }

    #[test]
    fn recorded_files_are_attached_again_unless_gone_or_there() {
        let records = [
            rec("app", "/x/app.sqlite", "sqlite"),
            rec("gone", "/x/gone.sqlite", "sqlite"),
            rec("init", "/x/init.duckdb", "duckdb"),
            rec("other", "/x/other.duckdb", "duckdb"),
            rec("pg", "dbname=x", "postgres"),
        ];
        let p = plan(&[], &records, &["workspace".into(), "INIT".into()], |path| path != "/x/gone.sqlite");
        assert!(p.loads.is_empty());
        assert_eq!(p.attaches, [records[0].clone(), records[3].clone()]);
        let why: Vec<(&str, &str)> = p.unattached.iter().map(|u| (u.record.name.as_str(), u.reason.as_str())).collect();
        assert_eq!(
            why,
            [
                ("gone", "/x/gone.sqlite: no such file"),
                ("pg", "dbname=x: a postgres database, which the workspace does not attach"),
            ]
        );
    }

    #[test]
    fn the_record_is_rewritten_from_what_is_attached_at_the_stop() {
        // Nothing attached and no record: nothing is written, and the
        // workspace gains no schema.
        assert_eq!(rewrite(&[], &[], false), None);
        let app = rec("app", "/x/o'b.sqlite", "sqlite");
        let sql = rewrite(&[app.clone()], &["gone".into()], false).unwrap();
        assert_eq!(
            sql.split(";\n").collect::<Vec<_>>(),
            [
                "BEGIN",
                RECORD_DDL[0],
                RECORD_DDL[1],
                "DELETE FROM _harbor.attached WHERE name NOT IN ('app', 'gone')",
                app.insert_sql().as_str(),
                "COMMIT",
            ]
        );
        // Everything detached: the record is emptied, not dropped.
        let sql = rewrite(&[], &[], true).unwrap();
        assert!(sql.contains("DELETE FROM _harbor.attached;\n") && !sql.contains("INSERT"), "{sql}");
    }
}
