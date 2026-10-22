//! The workspace, made whole at every start.
//!
//! The workspace database (`harbor_common::paths::workspace`) holds views
//! over files and the record of database files attached in it
//! (`wire::open::Record`). A view lasts in the database's own catalog; an
//! `ATTACH` lasts only as long as the server that ran it, and a view that
//! reads with `ST_Read` reads only in a server that has loaded `spatial`,
//! which the engine does not load on demand. So a server that opens the
//! workspace, before it serves anyone, loads the extensions its views call
//! into and attaches each recorded file again ([`open`]), installing in the
//! background an extension that is not installed, `sqlite` among them, and
//! attaching there the SQLite files that wait on it; and when it stops it
//! rewrites the record from what is attached then ([`record`]), so an
//! `ATTACH` or `DETACH` from any client lasts as the client meant it.
//!
//! A recorded file that is gone, that the engine will not attach, or whose
//! catalog does not read once attached (not SQLite after all, or locked by
//! another program), is said in the server's log and in `/catalog`, as an
//! attached database with its reason in `error` ([`unattached`]); the
//! server starts all the same, and the row stays, so the file is attached
//! again at a later start once it reads. A row deleted from the record is
//! forgotten. A SQLite file whose catalog stops reading for good while the
//! server runs, gone or not SQLite, is detached by the catalog read that
//! finds it, on any server, and set aside ([`set_aside`]); one this server
//! attached from the record is attached again by a later read once it
//! reads ([`ours`]), and the record written at the stop holds either as
//! attached. One that is only busy is not detached.

use std::sync::Mutex;

use wire::open::{self, Record, RECORD_SCHEMA, RECORD_TABLE};

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
/// refused, as the server never records one, and so is a relative path.
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
            _ if !absolute(&record.path) => format!(
                "{}: not an absolute path, which the workspace does not attach, since it would name a file \
                 in whichever directory a server starts in",
                record.path
            ),
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

/// Whether a recorded path names one file wherever a server starts: rooted,
/// or led by a drive.
fn absolute(path: &str) -> bool {
    let b = path.as_bytes();
    path.starts_with(['/', '\\'])
        || (b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'/' | b'\\'))
}

/// The statements that rewrite the record when the server stops: a row for
/// each of `current`, the databases attached then, and no row for anything
/// else but `kept`, the names the server could not attach when it started,
/// whose rows stay as they are. In one transaction, after the record is
/// made where it is not; nothing when nothing is attached and the record
/// does not exist. The record is named in `catalog`, the workspace's own.
pub fn rewrite(current: &[Record], kept: &[String], exists: bool, catalog: &str) -> Option<String> {
    if current.is_empty() && !exists {
        return None;
    }
    let table = open::record_in(catalog);
    let mut keep: Vec<String> = current.iter().map(|r| open::quote_literal(&r.name)).collect();
    keep.extend(kept.iter().map(|name| open::quote_literal(name)));
    let mut sql: Vec<String> = open::record_ddl_in(catalog).into();
    sql.insert(0, "BEGIN".into());
    sql.push(match keep.is_empty() {
        true => format!("DELETE FROM {table}"),
        false => format!("DELETE FROM {table} WHERE name NOT IN ({})", keep.join(", ")),
    });
    sql.extend(current.iter().map(|r| r.insert_into(&table)));
    sql.push("COMMIT".into());
    Some(sql.join(";\n"))
}

/// Set when this server serves the workspace: what it could not attach.
static SERVING: Mutex<Option<Vec<Unattached>>> = Mutex::new(None);

/// The databases attached before the workspace was opened, by the
/// operator's `--init`: theirs to attach at every start, so never recorded,
/// and a recorded row of one of their names stays as it is.
static INIT: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Whether this server serves the workspace.
pub fn serving() -> bool {
    SERVING.lock().unwrap().is_some()
}

/// What this server could not attach when it started, for `/catalog`.
pub fn unattached() -> Vec<Unattached> {
    SERVING.lock().unwrap().clone().unwrap_or_default()
}

/// A SQLite file a catalog read found attached and not reading for good,
/// and detached, on any server: the engine reads every attached database's
/// catalog together, so one that does not read fails every catalog read.
/// Listed in `/catalog` with why; attached again by the first catalog read
/// that finds it reads when this server attached it from the record
/// ([`ours`]), and otherwise left for a client to attach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetAside {
    pub unattached: Unattached,
    /// Its name was in the workspace's record when it was set aside: a row
    /// deleted since forgets it, as it forgets a file attached.
    pub recorded: bool,
    /// This server attached it from the record ([`ours`]), so attaching it
    /// again as the record does keeps it as it was.
    pub ours: bool,
}

static SET_ASIDE: Mutex<Vec<SetAside>> = Mutex::new(Vec::new());

/// The names this server attached from the record, at its start or in the
/// background, and no client has detached since.
static OURS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Held by a catalog read while it attaches a file set aside again, and
/// for a moment by a client's `ATTACH` or `DETACH` of its name as it is
/// claimed once it ran ([`claimed`]), so one does not undo the other.
static TAKING: Mutex<()> = Mutex::new(());

/// The names a client's `ATTACH` or `DETACH` names, from before it runs
/// until it is claimed or found not to have run: a catalog read does not
/// attach a file set aside under one of them again. Once for each
/// statement, so two of one name are each counted.
static PENDING: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Whether this server attached `name` from the record, with the options
/// the record attaches it with, and no client has detached it since.
pub fn ours(name: &str) -> bool {
    OURS.lock().unwrap().iter().any(|n| n.eq_ignore_ascii_case(name))
}

/// Held while a file set aside is attached again ([`TAKING`]).
pub fn taking() -> std::sync::MutexGuard<'static, ()> {
    TAKING.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A client's `ATTACH` or `DETACH` of `name` is about to run: until it is
/// [`settled`], a file set aside under `name` is not attached again
/// ([`is_pending`]). A file being attached again at this moment and the
/// statement run one after the other, at the catalog gate, and whichever
/// runs second finds what the first did.
pub fn pending(name: &str) {
    PENDING.lock().unwrap().push(name.to_string());
}

/// Whether a client's `ATTACH` or `DETACH` of `name` is pending, asked
/// holding [`taking`].
pub fn is_pending(name: &str, _taking: &std::sync::MutexGuard<'static, ()>) -> bool {
    PENDING.lock().unwrap().iter().any(|n| n.eq_ignore_ascii_case(name))
}

/// A client's `ATTACH` or `DETACH` of `name`, [`pending`], is over, and
/// changed what `name` is when `ran`: then it is [`claimed`].
pub fn settled(name: &str, ran: bool) {
    if ran {
        claimed(name, &taking());
    }
    let mut pending = PENDING.lock().unwrap();
    if let Some(at) = pending.iter().position(|n| n == name) {
        pending.swap_remove(at);
    }
}

/// A client detached or attached `name`, holding [`taking`]: it is not
/// this server's to attach again, and a file set aside under it is
/// forgotten.
fn claimed(name: &str, _taking: &std::sync::MutexGuard<'static, ()>) {
    OURS.lock().unwrap().retain(|n| !n.eq_ignore_ascii_case(name));
    taken_back(name);
}

/// Set aside a file detached, in place of any of its name.
pub fn set_aside(aside: SetAside) {
    let mut all = SET_ASIDE.lock().unwrap();
    all.retain(|a| !a.unattached.record.name.eq_ignore_ascii_case(&aside.unattached.record.name));
    all.push(aside);
}

/// What is set aside.
pub fn set_aside_now() -> Vec<SetAside> {
    SET_ASIDE.lock().unwrap().clone()
}

/// Forget the file set aside as `name`: attached again, or its name taken.
pub fn taken_back(name: &str) {
    SET_ASIDE.lock().unwrap().retain(|a| !a.unattached.record.name.eq_ignore_ascii_case(name));
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
    let catalog = served(con);
    let init: Vec<String> = attached.into_iter().filter(|name| *name != catalog).collect();
    for record in records.iter().filter(|r| init.iter().any(|name| name.eq_ignore_ascii_case(&r.name))) {
        eprintln!(
            "harbor: workspace: {} is attached by --init, so the file the workspace records for it, {}, is not",
            record.name, record.path
        );
    }
    *INIT.lock().unwrap() = init;

    // Installed by whoever made the view, and loaded here from the disk. One
    // that is not installed (a fresh extension directory, an engine of
    // another version) is installed in the background, since an install is
    // a download, and a repository that does not answer would hold the
    // start, and every client waiting on it, for as long as a connection
    // takes to time out; and so is one the engine would install on demand,
    // as `LOAD` does and as attaching a SQLite file does `sqlite`. Until it
    // is loaded, the views that need it say so in the engine's words, and
    // the SQLite files that wait on it are listed with why.
    let mut install = Vec::new();
    for &extension in &plan.loads {
        match installed(con, extension) {
            true => {
                if let Err(e) = con.execute_batch(&format!("LOAD {extension}")) {
                    eprintln!("harbor: workspace: {extension} did not load: {e}");
                }
            }
            false => {
                eprintln!("harbor: workspace: {extension} is not installed, and is installed in the background");
                install.push(extension);
            }
        }
    }
    let sqlite = plan.attaches.iter().any(|r| r.storage == "sqlite") && !installed(con, "sqlite_scanner");
    let (later, at_start): (Vec<Record>, Vec<Record>) =
        plan.attaches.into_iter().partition(|r| sqlite && r.storage == "sqlite");
    if !later.is_empty() {
        eprintln!("harbor: workspace: sqlite is not installed, and is installed in the background");
        install.push("sqlite_scanner");
    }
    let mut unattached = plan.unattached;
    for record in at_start {
        match attach(con, &record) {
            Ok(()) => OURS.lock().unwrap().push(record.name.clone()),
            Err(reason) => unattached.push(Unattached { record, reason }),
        }
    }
    for u in &unattached {
        eprintln!("harbor: workspace: {} is not attached — {}", u.record.name, u.reason);
    }
    unattached.extend(later.iter().map(|record| Unattached { record: record.clone(), reason: WAITING.to_string() }));
    *SERVING.lock().unwrap() = Some(unattached);
    if !install.is_empty() {
        install_later(con, install, later);
    }
}

/// Why a recorded SQLite file is not attached while the extension that
/// attaches it is installed.
const WAITING: &str = "attached once sqlite, which is not installed here, is installed in the background";

/// Whether `extension` is installed, or loaded, as the engine finds it on
/// the disk, which asks no repository.
fn installed(con: &mut Conn, extension: &str) -> bool {
    con.query_strings(&format!(
        "SELECT (installed OR loaded)::VARCHAR FROM duckdb_extensions() WHERE extension_name = {}",
        open::quote_literal(extension)
    ))
    .is_ok_and(|rows| rows.first().is_some_and(|v| v == "true"))
}

/// Install and load `extensions` on a connection of their own, on a thread
/// of their own: a load is the whole instance's. Then attach `records`, the
/// SQLite files that waited on one of them, each alone against the
/// catalog's readers, as any `ATTACH` is. Said in the server's log, done or
/// not, and what does not attach in `/catalog`.
fn install_later(con: &Conn, extensions: Vec<&'static str>, records: Vec<Record>) {
    let named = extensions.join(", ");
    let failed = |records: &[Record], reason: String| {
        for record in records {
            attached_or(&record.name, Some(reason.clone()));
        }
    };
    let mut own = match con.try_clone() {
        Ok(own) => own,
        Err(e) => {
            eprintln!("harbor: workspace: no connection to install {named} on: {e}");
            failed(&records, format!("no connection to install sqlite on: {e}"));
            return;
        }
    };
    let waiting = records.clone();
    let spawned = std::thread::Builder::new().name("harbor-install".to_string()).spawn(move || {
        let mut sqlite = Ok(());
        for extension in extensions {
            match own.execute_batch(&format!("INSTALL {extension}; LOAD {extension}")) {
                Ok(()) => eprintln!("harbor: workspace: {extension} installed and loaded"),
                Err(e) => {
                    eprintln!("harbor: workspace: {extension} did not install, and what needs it says so: {e}");
                    if extension == "sqlite_scanner" {
                        sqlite = Err(format!("sqlite did not install: {e}"));
                    }
                }
            }
        }
        for record in records {
            let attached = match &sqlite {
                Ok(()) => {
                    let alone = crate::changing_catalog();
                    let attached = attach(&mut own, &record);
                    crate::learn_again(alone);
                    attached
                }
                Err(reason) => Err(reason.clone()),
            };
            match &attached {
                Ok(()) => {
                    OURS.lock().unwrap().push(record.name.clone());
                    eprintln!("harbor: workspace: {} is attached", record.name)
                }
                Err(reason) => eprintln!("harbor: workspace: {} is not attached — {reason}", record.name),
            }
            attached_or(&record.name, attached.err());
        }
    });
    if let Err(e) = spawned {
        eprintln!("harbor: workspace: no thread to install {named} on: {e}");
        failed(&waiting, format!("no thread to install sqlite on: {e}"));
    }
}

/// The recorded file `name` attached, given `None`, or not, and why.
fn attached_or(name: &str, reason: Option<String>) {
    let mut serving = SERVING.lock().unwrap();
    let Some(unattached) = serving.as_mut() else { return };
    match reason {
        None => unattached.retain(|u| u.record.name != name),
        Some(reason) => {
            for u in unattached.iter_mut().filter(|u| u.record.name == name) {
                u.reason = reason.clone();
            }
        }
    }
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

/// The served database's name, which names the record ([`open::record_in`]).
fn served(con: &mut Conn) -> String {
    con.query_strings("SELECT current_database()").ok().and_then(|mut rows| rows.pop()).unwrap_or_default()
}

/// The record's rows, or none when it has not been made.
fn read_records(con: &mut Conn) -> Vec<Record> {
    if !record_exists(con) {
        return Vec::new();
    }
    let table = open::record_in(&served(con));
    let rows = con.query_strings(&format!(
        "SELECT json_object('name', name, 'path', path, 'type', type, 'read_only', read_only)::VARCHAR \
         FROM {table} ORDER BY name"
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
/// ([`rewrite`]), less what `--init` attached, whose rows stay as they are.
/// A failure is said and leaves the record as it was.
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
    let init = INIT.lock().unwrap().clone();
    let by_init = |name: &str| init.iter().any(|n| n.eq_ignore_ascii_case(name));
    let current: Vec<Record> = match current {
        Ok(rows) => rows
            .iter()
            .filter_map(|row| serde_json::from_str::<Record>(row).ok())
            .filter(|r| Record::kept(&r.storage, &r.path) && !by_init(&r.name))
            .collect(),
        Err(e) => {
            eprintln!("harbor: workspace: what is attached could not be read, and the record stays as it was: {e}");
            return;
        }
    };
    // A file set aside is recorded as attached, as it was until it stopped
    // reading, unless its row was deleted since.
    let rows: Vec<String> = read_records(con).into_iter().map(|r| r.name).collect();
    let mut current = current;
    for aside in set_aside_now() {
        let record = aside.unattached.record;
        let named = |name: &String| name.eq_ignore_ascii_case(&record.name);
        if (aside.recorded && !rows.iter().any(named)) || current.iter().any(|r| named(&r.name)) || by_init(&record.name) {
            continue;
        }
        current.push(record);
    }
    current.sort_by(|a, b| a.name.cmp(&b.name));
    let kept: Vec<String> = unattached
        .into_iter()
        .map(|u| u.record.name)
        .filter(|name| !current.iter().any(|r| r.name.eq_ignore_ascii_case(name)))
        .chain(init)
        .collect();
    let exists = record_exists(con);
    let catalog = served(con);
    let Some(sql) = rewrite(&current, &kept, exists, &catalog) else { return };
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
            rec("rel", "mk.duckdb", "duckdb"),
            rec("win", "C:\\x\\w.sqlite", "sqlite"),
        ];
        let p = plan(&[], &records, &["workspace".into(), "INIT".into()], |path| path != "/x/gone.sqlite");
        assert!(p.loads.is_empty());
        assert_eq!(p.attaches, [records[0].clone(), records[3].clone(), records[6].clone()]);
        let why: Vec<(&str, &str)> = p.unattached.iter().map(|u| (u.record.name.as_str(), u.reason.as_str())).collect();
        assert_eq!(
            why,
            [
                ("gone", "/x/gone.sqlite: no such file"),
                ("pg", "dbname=x: a postgres database, which the workspace does not attach"),
                (
                    "rel",
                    "mk.duckdb: not an absolute path, which the workspace does not attach, since it would \
                     name a file in whichever directory a server starts in"
                ),
            ]
        );
    }

    #[test]
    fn the_record_is_rewritten_from_what_is_attached_at_the_stop() {
        // Nothing attached and no record: nothing is written, and the
        // workspace gains no schema.
        assert_eq!(rewrite(&[], &[], false, "workspace"), None);
        let app = rec("app", "/x/o'b.sqlite", "sqlite");
        let sql = rewrite(std::slice::from_ref(&app), &["gone".into()], false, "workspace").unwrap();
        let ddl = open::record_ddl_in("workspace");
        assert_eq!(
            sql.split(";\n").collect::<Vec<_>>(),
            [
                "BEGIN",
                ddl[0].as_str(),
                ddl[1].as_str(),
                "DELETE FROM \"workspace\"._harbor.attached WHERE name NOT IN ('app', 'gone')",
                app.insert_into("\"workspace\"._harbor.attached").as_str(),
                "COMMIT",
            ]
        );
        // Everything detached: the record is emptied, not dropped.
        let sql = rewrite(&[], &[], true, "workspace").unwrap();
        assert!(sql.contains("DELETE FROM \"workspace\"._harbor.attached;\n") && !sql.contains("INSERT"), "{sql}");
    }
}
