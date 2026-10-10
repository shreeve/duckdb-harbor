//! Probes against a live Harbor. Ignored by default: they need a server and
//! say nothing in CI. They create and drop tables, so they run only against
//! the database file `HARBOR_LIVE_DB` names, and skip without it; nothing is
//! ever chosen from the machine's fleet. Run explicitly:
//! `HARBOR_HOME=/tmp/dt-home HARBOR_LIVE_DB=/tmp/scratch.duckdb cargo test -p harbor-client --test live -- --ignored --nocapture`
//!
//! One probe takes another variable: `an_open_database_outlives_harbors_linger`
//! starts a server of its own on a copy of the file `HARBOR_FIXTURE` names
//! (its own comment has the command), and fails without it.
//!
//! They share one server, its handful of session connections and a few
//! tables, so each probe takes its turn (`scratch`), whatever
//! `--test-threads` says. The two that read the fleet run only under a
//! `HARBOR_HOME` of their own, so they never survey the machine's databases.

use harbor_client::{fleet, info, Conn};

/// One probe at a time on the scratch server.
static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

thread_local! {
    /// This test's thread holds the turn: a second connection in the same
    /// probe does not wait for it.
    static HOLDS_TURN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A connection to the scratch database, and the probe's turn on it.
struct Scratch {
    conn: Conn,
    turn: Option<std::sync::MutexGuard<'static, ()>>,
}

impl std::ops::Deref for Scratch {
    type Target = Conn;
    fn deref(&self) -> &Conn {
        &self.conn
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if self.turn.is_some() {
            HOLDS_TURN.set(false);
        }
    }
}

/// The scratch database's file, when `HARBOR_LIVE_DB` names one.
fn scratch_db() -> Option<std::path::PathBuf> {
    let db = std::env::var_os("HARBOR_LIVE_DB").map(std::path::PathBuf::from);
    if db.is_none() {
        println!("set HARBOR_LIVE_DB to a scratch database file; skipping");
    }
    db
}

/// A connection to the scratch database `HARBOR_LIVE_DB` names, by its path:
/// its server is joined, or started on demand. None, and the probe skips,
/// when the variable is not set. The path is the whole choice: a probe that
/// writes must never land on whatever database happens to be running.
fn scratch() -> Option<Scratch> {
    let db = scratch_db()?;
    // A probe that failed while it held the turn leaves it poisoned, which
    // says nothing about the next one.
    let turn = (!HOLDS_TURN.replace(true)).then(|| TURN.lock().unwrap_or_else(|p| p.into_inner()));
    let conn = fleet::connect_path(&db).expect("connect");
    println!("database: {}", db.display());
    Some(Scratch { conn, turn })
}

/// Whether the fleet may be surveyed: only under a `HARBOR_HOME` set for the
/// probes, where the scratch servers are the whole fleet. Without it a
/// survey would read every database this machine serves.
fn own_fleet() -> bool {
    let own = std::env::var_os("HARBOR_HOME").is_some();
    if !own {
        println!("set HARBOR_HOME to a scratch directory to survey the fleet; skipping");
    }
    own
}

#[test]
#[ignore]
fn the_fleet_lists_the_scratch_database() {
    if !own_fleet() {
        return;
    }
    let Some(_conn) = scratch() else { return };
    let db = fleet_path(&scratch_db().unwrap());
    let fleet = fleet::survey();
    if let Some(w) = &fleet.warning {
        println!("warning: {w}");
    }
    println!("fleet:");
    for row in &fleet.rows {
        println!("  {} {}", row.state.label(), row.name);
        if let Some(note) = &row.note {
            println!("    note: {note}");
        }
    }
    let row = fleet.rows.iter().find(|r| r.path.as_deref().map(fleet_path) == Some(db.clone()));
    assert!(row.is_some_and(|r| r.state.is_live()), "the scratch database is not a running row");
}

#[test]
#[ignore]
fn a_database_saved_twice_at_one_address_is_saved_once() {
    if !own_fleet() {
        return;
    }
    let name = format!("twice-{}", std::process::id());
    // Two saves of one aim at once, the way a redial overtakes the save
    // of the dial it replaces: both are the database saved.
    let saves: Vec<_> = (0..2)
        .map(|_| {
            let name = name.clone();
            std::thread::spawn(move || fleet::add_database(&name, "localhost", "9611"))
        })
        .collect();
    for save in saves {
        assert_eq!(save.join().unwrap(), Ok(name.clone()));
    }
    assert_eq!(fleet::add_database(&name, "127.0.0.1", "9611"), Ok(name.clone()));
    let other = fleet::add_database(&name, "localhost", "9612");
    assert!(other.is_err_and(|e| e.contains("already exists")));
    fleet::remove_remote(&name).unwrap();
}

/// A database file's path as the fleet compares it: canonical.
fn fleet_path(db: &std::path::Path) -> std::path::PathBuf {
    harbor_client::paths::canonical_db(db).expect("a database path")
}

#[test]
#[ignore]
fn a_live_berth_yields_identity() {
    let Some(conn) = scratch() else { return };
    let identity = info(&conn).expect("info");
    println!(
        "{}: duckdb {} harbor {} db {}",
        identity.name, identity.duckdb_version, identity.harbor_version, identity.database
    );
    // The server is the one on the scratch file. Its name is its own to
    // choose: the config's, when the file is attached under another.
    assert_eq!(fleet_path(std::path::Path::new(&identity.database)), fleet_path(&scratch_db().unwrap()));
}

#[test]
#[ignore]
fn a_live_berth_answers_sql() {
    let Some(conn) = scratch() else { return };
    let result = harbor_client::query(&conn, "SELECT 1 AS one, 'two' AS two, NULL AS three")
        .expect("query");
    println!(
        "columns: {:?}",
        result.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>()
    );
    println!("rows: {:?} ({} in {} ms)", result.rows, result.row_count, result.time_ms);
    assert_eq!(result.columns.len(), 3);
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], serde_json::json!(1));
    assert_eq!(result.rows[0][2], serde_json::Value::Null);
}

/// The File→Open lifetime regression: with a sub-second Harbor linger, pause
/// well beyond zero clients and prove the same DuckTable connection still
/// answers. Run with:
/// `HARBOR_BIN="$(pwd)/../harbor/target/debug/harbor" HARBOR_FIXTURE="$(pwd)/../harbor/sample.duckdb" HARBOR_LINGER_MS=500 cargo test -p harbor-client --test live an_open_database_outlives_harbors_linger -- --ignored`
#[test]
#[ignore]
fn an_open_database_outlives_harbors_linger() {
    let fixture = std::env::var("HARBOR_FIXTURE").expect("set HARBOR_FIXTURE");
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let db = std::env::temp_dir().join(format!("ducktable-anchor-{unique}.duckdb"));
    std::fs::copy(fixture, &db).expect("copy fixture");
    let socket = harbor_common::paths::socket_for(
        &harbor_common::paths::runtime_dir().expect("runtime dir"),
        &db,
    )
    .expect("socket path");

    let conn = fleet::connect_path(&db).expect("connect_path");
    harbor_client::query(&conn, "SELECT 1").expect("first query");
    std::thread::sleep(std::time::Duration::from_secs(2));
    harbor_client::query(&conn, "SELECT 2").expect("query after Harbor linger");
    drop(conn);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while socket.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(!socket.exists(), "ephemeral Harbor stayed after its connection closed");
    let _ = std::fs::remove_file(db);
}

/// The exact wire sequence the GUI's ⌘S performs (docs/EDITING.md):
/// a session pins one connection, BEGIN..COMMIT spans requests on it,
/// parameters bind instead of concatenating, and UPDATE answers with a
/// count row — the affected-exactly-one verification reads that cell.
#[test]
#[ignore]
fn a_session_carries_a_transaction_with_bound_params() {
    use serde_json::json;
    let Some(conn) = scratch() else { return };
    let sid = harbor_client::session_new(&conn).expect("session");
    let run = |sql: &str, params: Option<Vec<serde_json::Value>>| {
        harbor_client::exec(&conn, sql, params, Some(&sid)).expect(sql)
    };
    // TEMP tables stay on the pinned worker rather than entering the
    // database's real schema. A released session returns that connection
    // to Harbor's pool, so clean up names explicitly for repeatability.
    run("DROP TABLE IF EXISTS _dt_edit_probe", None);
    run("DROP TABLE IF EXISTS _dt_insert_probe", None);
    run("DROP TABLE IF EXISTS _dt_default_probe", None);
    run("CREATE TEMP TABLE _dt_edit_probe(id INTEGER PRIMARY KEY, name VARCHAR)", None);
    run("INSERT INTO _dt_edit_probe VALUES (?, ?), (?, ?)",
        Some(vec![json!(1), json!("a"), json!(2), json!("b")]));
    // The staged-update shape: SET by param, WHERE binds the ORIGINAL key.
    run("BEGIN", None);
    let hit = run("UPDATE _dt_edit_probe SET \"name\" = ? WHERE \"id\" = ?",
        Some(vec![json!("z"), json!(1)]));
    println!("update answered: {:?}", hit.rows);
    assert_eq!(hit.rows[0][0].as_u64(), Some(1), "one row, exactly");
    // A WHERE that no longer matches answers 0 — the signal the commit
    // guard turns into a full rollback.
    let miss = run("UPDATE _dt_edit_probe SET \"name\" = ? WHERE \"id\" = ?",
        Some(vec![json!("q"), json!(99)]));
    assert_eq!(miss.rows[0][0].as_u64(), Some(0), "a vanished row answers 0");
    run("COMMIT", None);
    let after = run("SELECT name FROM _dt_edit_probe ORDER BY id", None);
    assert_eq!(after.rows[0][0], json!("z"));
    // And the rollback leg: BEGIN, change, ROLLBACK — nothing landed.
    run("BEGIN", None);
    run("UPDATE _dt_edit_probe SET name = ? WHERE id = ?", Some(vec![json!("gone"), json!(2)]));
    run("ROLLBACK", None);
    let intact = run("SELECT name FROM _dt_edit_probe WHERE id = 2", None);
    assert_eq!(intact.rows[0][0], json!("b"), "rollback left the row untouched");

    // The staged-INSERT shape: omitted columns keep DEFAULT semantics,
    // values bind, and RETURNING exposes the engine-computed truth.
    run(
        "CREATE TEMP TABLE _dt_insert_probe(\
         id INTEGER PRIMARY KEY DEFAULT 41, \
         name VARCHAR NOT NULL, \
         doubled INTEGER GENERATED ALWAYS AS (id * 2))",
        None,
    );
    run("BEGIN", None);
    let inserted = run(
        "INSERT INTO _dt_insert_probe (name) VALUES (?) RETURNING *",
        Some(vec![json!("Ada")]),
    );
    assert_eq!(inserted.rows, vec![vec![json!(41), json!("Ada"), json!(82)]]);
    run("COMMIT", None);
    run("CREATE TEMP TABLE _dt_default_probe(answer INTEGER DEFAULT 42)", None);
    let defaults = run("INSERT INTO _dt_default_probe DEFAULT VALUES RETURNING *", None);
    assert_eq!(defaults.rows, vec![vec![json!(42)]]);
    run("DROP TABLE _dt_edit_probe", None);
    run("DROP TABLE _dt_insert_probe", None);
    run("DROP TABLE _dt_default_probe", None);
    harbor_client::session_release(&conn, &sid);
    println!("session {sid}: transaction, params, counts — all as the spec assumes");
}

#[test]
#[ignore]
fn keyless_base_tables_expose_rowid() {
    let Some(conn) = scratch() else { return };
    // Find any base table, then probe the rowid pseudocolumn through
    // the same wire the grid would use.
    let tables = harbor_client::query(
        &conn,
        "SELECT schema_name, table_name FROM duckdb_tables() LIMIT 1",
    )
    .expect("duckdb_tables");
    let Some(t) = tables.rows.first() else {
        println!("no base tables; skipping");
        return;
    };
    let (schema, name) = (t[0].as_str().unwrap(), t[1].as_str().unwrap());
    let sql = format!(
        "SELECT [rowid::UBIGINT, hash(*COLUMNS(*))] AS rowid, * FROM \"{schema}\".\"{name}\" LIMIT 3"
    );
    let result = harbor_client::query(&conn, &sql).expect("rowid probe");
    println!("rowid probe on {schema}.{name}:");
    println!("  columns: {:?}", result.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>());
    for r in &result.rows {
        println!("  {:?}", r.first());
    }
    assert!(result.columns.first().and_then(|c| c.name.as_deref()) == Some("rowid"));
    // And the shape an UPDATE would use: the rowid and the aliased row's
    // hash in the WHERE.
    let sql = format!(
        "SELECT count(*) FROM \"{schema}\".\"{name}\" AS \"row\" WHERE \"rowid\" = 0 AND hash(\"row\") = 0::UBIGINT"
    );
    let count = harbor_client::query(&conn, &sql).expect("rowid and hash in WHERE");
    println!("  quoted-WHERE count row: {:?}", count.rows.first());
}

/// The placeholders DuckTable binds a VARIANT, a JSON and a BLOB cell
/// through (ducktable's `edits.rs`), against the engine, in the session
/// transaction a commit runs in. A bare `?` stores a VARIANT string whose
/// every path is NULL, and stores a BLOB's base64 characters as its bytes;
/// this is the proof that the typed forms do not.
#[test]
#[ignore]
fn document_and_blob_cells_bind_as_what_they_are() {
    use serde_json::json;
    let Some(conn) = scratch() else { return };
    let sid = harbor_client::session_new(&conn).expect("session");
    let run = |sql: &str, params: Option<Vec<serde_json::Value>>| {
        harbor_client::exec(&conn, sql, params, Some(&sid)).expect(sql)
    };
    run("DROP TABLE IF EXISTS _dt_typed_probe", None);
    run("DROP TABLE IF EXISTS _dt_blobkey_probe", None);
    run("CREATE TEMP TABLE _dt_typed_probe(id INTEGER PRIMARY KEY, doc VARIANT, j JSON, b BLOB)", None);
    run("INSERT INTO _dt_typed_probe VALUES (1, '{\"a\":{\"b\":1}}'::JSON, '[1]', '\\xAA\\xBB'::BLOB)", None);

    run("BEGIN", None);
    let hit = run(
        "UPDATE _dt_typed_probe SET \"doc\" = ?::JSON, \"j\" = ?::JSON, \"b\" = from_base64(?::VARCHAR) WHERE \"id\" = ?",
        Some(vec![json!("{\"a\":{\"b\":2}}"), json!("{\"k\": null}"), json!("AAECAw=="), json!(1)]),
    );
    assert_eq!(hit.rows[0][0].as_u64(), Some(1), "one row, exactly");
    // The draft-row shape, with the NULLs a cleared cell binds.
    let made = run(
        "INSERT INTO _dt_typed_probe (\"id\", \"doc\", \"j\", \"b\") VALUES (?, ?::JSON, ?::JSON, from_base64(?::VARCHAR)) RETURNING *",
        Some(vec![json!(2), serde_json::Value::Null, serde_json::Value::Null, serde_json::Value::Null]),
    );
    assert_eq!(made.rows.len(), 1, "RETURNING answers the one row");
    // A quoted string is a string, a bare number a number.
    run(
        "INSERT INTO _dt_typed_probe (\"id\", \"doc\") VALUES (?, ?::JSON), (?, ?::JSON)",
        Some(vec![json!(3), json!("\"Morel\""), json!(4), json!("42")]),
    );
    run("COMMIT", None);

    let after = run(
        "SELECT id, variant_typeof(doc), doc.a.b::VARCHAR, doc IS NULL, j, j IS NULL, \
         octet_length(b), b = '\\x00\\x01\\x02\\x03'::BLOB, b IS NULL \
         FROM _dt_typed_probe ORDER BY id",
        None,
    );
    println!("typed cells: {:?}", after.rows);
    assert_eq!(after.rows[0][1], json!("OBJECT(a)"), "a document, not a string");
    assert_eq!(after.rows[0][2], json!("2"), "and its paths read");
    assert_eq!(after.rows[0][4], json!("{\"k\": null}"), "a JSON column keeps its text");
    assert_eq!(after.rows[0][6].as_u64(), Some(4), "four bytes, not eight base64 characters");
    assert_eq!(after.rows[0][7], json!(true));
    assert_eq!(after.rows[1][3], json!(true), "a NULL param through ?::JSON is SQL NULL");
    assert_eq!(after.rows[1][5], json!(true));
    assert_eq!(after.rows[1][8], json!(true), "and through from_base64(?::VARCHAR)");
    assert_eq!(after.rows[2][1], json!("VARCHAR"));
    assert_eq!(after.rows[3][1], json!("UINT64"));

    // A BLOB key: the WHERE decodes it too, so the row named is the row hit.
    // Row B's bytes are the characters of row A's base64.
    run("CREATE TEMP TABLE _dt_blobkey_probe(k BLOB PRIMARY KEY, name VARCHAR)", None);
    run("INSERT INTO _dt_blobkey_probe VALUES ('\\x00\\x01'::BLOB, 'A'), ('AAE='::BLOB, 'B')", None);
    let hit = run(
        "UPDATE _dt_blobkey_probe SET \"name\" = ? WHERE \"k\" = from_base64(?::VARCHAR)",
        Some(vec![json!("hit"), json!("AAE=")]),
    );
    assert_eq!(hit.rows[0][0].as_u64(), Some(1));
    let names = run("SELECT name FROM _dt_blobkey_probe ORDER BY octet_length(k)", None);
    assert_eq!(names.rows[0][0], json!("hit"), "row A, the two bytes");
    assert_eq!(names.rows[1][0], json!("B"), "row B untouched");

    run("DROP TABLE _dt_typed_probe", None);
    run("DROP TABLE _dt_blobkey_probe", None);
}

/// The statement Duplicate Row commits (ducktable's `edits.rs`): untouched
/// cells are selected from the source row, a typed-over cell is bound, and
/// the WHERE names the source by its key or its rowid. The wire is narrower
/// than the engine — a VARIANT crosses it as JSON, a BLOB[] as base64 text,
/// an INTERVAL as an object, a MAP as pairs — so a copy that rebinds what
/// was fetched is a different value; this is the proof that the copy made
/// in SQL is not, in the session transaction a commit runs in.
#[test]
#[ignore]
fn a_duplicate_row_copies_in_sql_what_the_wire_cannot_carry() {
    use serde_json::json;
    let Some(conn) = scratch() else { return };
    let sid = harbor_client::session_new(&conn).expect("session");
    let run = |sql: &str, params: Option<Vec<serde_json::Value>>| {
        harbor_client::exec(&conn, sql, params, Some(&sid)).expect(sql)
    };
    run("DROP TABLE IF EXISTS _dt_dup_probe", None);
    run("DROP TABLE IF EXISTS _dt_dup_keyless_probe", None);
    run("DROP SEQUENCE IF EXISTS _dt_dup_seq", None);
    run("CREATE TEMP SEQUENCE _dt_dup_seq START 100", None);
    run(
        "CREATE TEMP TABLE _dt_dup_probe(id INTEGER PRIMARY KEY DEFAULT nextval('_dt_dup_seq'), \
         name VARCHAR, doc VARIANT, bl BLOB[], iv INTERVAL, m MAP(VARCHAR, INTEGER), \
         u UNION(n INTEGER, s VARCHAR), docs VARIANT[])",
        None,
    );
    run(
        "INSERT INTO _dt_dup_probe VALUES (1, 'source', \
         {'day': DATE '2024-02-29', 'big': 170141183460469231731687303715884105727::HUGEINT}::VARIANT, \
         ['hi'::BLOB, '\\xFF'::BLOB], INTERVAL '14 months 2 days 3 seconds', MAP {'a': 1, 'b': 2}, \
         union_value(s := '7'), [DATE '2024-02-29'::VARIANT, 12.340::DECIMAL(10,3)::VARIANT])",
        None,
    );

    // What the grid fetched, and what rebinding it makes of the document.
    let wire = run("SELECT doc, bl, iv, m, u, docs FROM _dt_dup_probe WHERE id = 1", None);
    println!("the source row on the wire: {:?}", wire.rows[0]);

    run("BEGIN", None);
    let rebound = run(
        "INSERT INTO _dt_dup_probe (\"name\", \"doc\") VALUES (?, ?::JSON) RETURNING id",
        Some(vec![json!("rebound"), wire.rows[0][0].clone()]),
    );
    // The duplicate: `name` typed over, everything else read from the source.
    let copied = run(
        "INSERT INTO _dt_dup_probe (\"name\", \"doc\", \"bl\", \"iv\", \"m\", \"u\", \"docs\") \
         SELECT ?, \"doc\", \"bl\", \"iv\", \"m\", \"u\", \"docs\" FROM _dt_dup_probe WHERE \"id\" = ? RETURNING *",
        Some(vec![json!("copy"), json!(1)]),
    );
    assert_eq!(copied.rows.len(), 1, "RETURNING answers the one row");
    assert_eq!(copied.rows[0][1], json!("copy"), "the bound cell is the typed one");
    // A source row that is gone returns nothing, which commit refuses; a
    // scalar subquery in VALUES would insert a row of NULLs instead.
    let gone = run(
        "INSERT INTO _dt_dup_probe (\"name\", \"doc\") SELECT ?, \"doc\" FROM _dt_dup_probe WHERE \"id\" = ? RETURNING *",
        Some(vec![json!("orphan"), json!(999)]),
    );
    assert!(gone.rows.is_empty(), "no source row, no insert");
    let nulls = run(
        "INSERT INTO _dt_dup_probe (\"name\", \"doc\") VALUES (?, (SELECT \"doc\" FROM _dt_dup_probe WHERE \"id\" = ?)) RETURNING doc IS NULL",
        Some(vec![json!("orphan"), json!(999)]),
    );
    println!("a gone source: selected from, {:?}; as a scalar subquery, {:?}", gone.rows, nulls.rows);
    assert_eq!(nulls.rows, vec![vec![json!(true)]], "the shape not used lands a NULL and says nothing");

    let compare = |id: &serde_json::Value| {
        run(
            "SELECT c.name, c.doc = s.doc, variant_typeof(c.doc.day), variant_typeof(c.doc.big), \
             c.bl IS NOT DISTINCT FROM s.bl, c.iv IS NOT DISTINCT FROM s.iv, \
             c.m IS NOT DISTINCT FROM s.m, c.u IS NOT DISTINCT FROM s.u, \
             c.docs IS NOT DISTINCT FROM s.docs, variant_typeof(c.docs[1]), variant_typeof(c.docs[2]) \
             FROM _dt_dup_probe c, _dt_dup_probe s WHERE s.id = 1 AND c.id = ?",
            Some(vec![id.clone()]),
        )
        .rows
        .remove(0)
    };
    let source = compare(&json!(1));
    let rebound = compare(&rebound.rows[0][0]);
    let copy = compare(&copied.rows[0][0]);
    println!("source:  {source:?}");
    println!("rebound: {rebound:?}");
    println!("copy:    {copy:?}");
    assert_eq!(source[2], json!("DATE"));
    assert_eq!(source[3], json!("INT128"));
    assert_eq!(rebound[2], json!("VARCHAR"), "JSON has no DATE, so the wire's copy is a string");
    assert_ne!(rebound[3], source[3], "nor a 128-bit integer");
    assert_eq!(copy[1..], source[1..], "the copy made in SQL is the source, type for type");
    assert!(copy[1..].iter().all(|v| v.as_bool() != Some(false)));

    // The source row is deleted in the same transaction, after the insert
    // read it: deletes run last.
    let hit = run("DELETE FROM _dt_dup_probe WHERE \"id\" = ?", Some(vec![json!(1)]));
    assert_eq!(hit.rows[0][0].as_u64(), Some(1));
    run("COMMIT", None);
    let kept = run(
        "SELECT name, variant_typeof(doc.day), doc.big::VARCHAR, iv::VARCHAR, m::VARCHAR, \
         union_tag(u)::VARCHAR, octet_length(bl[2]) FROM _dt_dup_probe WHERE name = 'copy'",
        None,
    );
    println!("the copy, its source deleted: {:?}", kept.rows);
    assert_eq!(
        kept.rows,
        vec![vec![
            json!("copy"),
            json!("DATE"),
            json!("170141183460469231731687303715884105727"),
            json!("1 year 2 months 2 days 00:00:03"),
            json!("{a=1, b=2}"),
            json!("s"),
            json!(1),
        ]]
    );

    // A keyless table names the source by its rowid.
    run("CREATE TEMP TABLE _dt_dup_keyless_probe(name VARCHAR, doc VARIANT)", None);
    run("INSERT INTO _dt_dup_keyless_probe VALUES ('a', DATE '2020-01-01'::VARIANT)", None);
    let source_rowid = run("SELECT rowid FROM _dt_dup_keyless_probe", None).rows.remove(0).remove(0);
    let copied = run(
        "INSERT INTO _dt_dup_keyless_probe (\"name\", \"doc\") SELECT \"name\", \"doc\" \
         FROM _dt_dup_keyless_probe WHERE \"rowid\" = ? RETURNING *",
        Some(vec![source_rowid]),
    );
    assert_eq!(copied.rows.len(), 1);
    let types = run("SELECT name, variant_typeof(doc) FROM _dt_dup_keyless_probe ORDER BY rowid", None);
    println!("keyless, by rowid: {:?}", types.rows);
    assert_eq!(types.rows[0], types.rows[1]);
    assert_eq!(types.rows[1][1], json!("DATE"));

    run("DROP TABLE _dt_dup_probe", None);
    run("DROP TABLE _dt_dup_keyless_probe", None);
    run("DROP SEQUENCE _dt_dup_seq", None);
}

/// What DuckTable's `parse_value` binds for a number JSON cannot carry: an
/// integer past 64 bits as its digits, a DOUBLE that is not finite by name.
/// The engine casts both exactly, and refuses `''` for an ENUM and a UUID,
/// which is why neither clears to the empty string.
#[test]
#[ignore]
fn wide_integers_and_non_finite_doubles_bind_as_text() {
    use serde_json::json;
    let Some(conn) = scratch() else { return };
    let sid = harbor_client::session_new(&conn).expect("session");
    let run = |sql: &str, params: Option<Vec<serde_json::Value>>| {
        harbor_client::exec(&conn, sql, params, Some(&sid)).expect(sql)
    };
    run("DROP TABLE IF EXISTS _dt_number_probe", None);
    run(
        "CREATE TEMP TABLE _dt_number_probe(k INTEGER, h HUGEINT, ub UBIGINT, uh UHUGEINT, \
         d DOUBLE, f FLOAT, e ENUM('POINT', 'LINE'), u UUID)",
        None,
    );
    let wide = [
        "170141183460469231731687303715884105727",
        "18446744073709551615",
        "340282366920938463463374607431768211455",
    ];
    let stored = run(
        "INSERT INTO _dt_number_probe (k, h, ub, uh) VALUES (?, ?, ?, ?) \
         RETURNING h::VARCHAR, ub::VARCHAR, uh::VARCHAR",
        Some(vec![json!(1), json!(wide[0]), json!(wide[1]), json!(wide[2])]),
    );
    println!("wide integers bound as text: {:?}", stored.rows[0]);
    assert_eq!(stored.rows[0], wide.map(|w| json!(w)).to_vec());
    let low = run(
        "UPDATE _dt_number_probe SET \"h\" = ? WHERE \"k\" = ?",
        Some(vec![json!("-170141183460469231731687303715884105728"), json!(1)]),
    );
    assert_eq!(low.rows[0][0].as_u64(), Some(1));

    for (name, shown, nan, inf) in [
        ("nan", "NaN", true, false),
        ("NaN", "NaN", true, false),
        ("inf", "Infinity", false, true),
        ("+inf", "Infinity", false, true),
        ("-inf", "-Infinity", false, true),
        ("Infinity", "Infinity", false, true),
        ("-Infinity", "-Infinity", false, true),
        ("infinity", "Infinity", false, true),
    ] {
        let r = run(
            "INSERT INTO _dt_number_probe (k, d, f) VALUES (?, ?, ?) RETURNING d, f, isnan(d), isinf(d), d IS NULL",
            Some(vec![json!(2), json!(name), json!(name)]),
        );
        println!("{name:?} into DOUBLE and FLOAT: {:?}", r.rows[0]);
        assert_eq!(r.rows[0], vec![json!(shown), json!(shown), json!(nan), json!(inf), json!(false)]);
    }

    for (col, ty) in [("e", "ENUM"), ("u", "UUID")] {
        let sql = format!("INSERT INTO _dt_number_probe (k, {col}) VALUES (?, ?)");
        let refused = harbor_client::exec(&conn, &sql, Some(vec![json!(3), json!("")]), Some(&sid));
        println!("'' into {ty}: {:?}", refused.as_ref().err());
        assert!(refused.is_err(), "'' is not a value of {ty}");
    }
    run("DROP TABLE _dt_number_probe", None);
}

/// A FLOAT key in the WHERE (ducktable's `edits.rs`, `key_placeholder_for`).
/// The wire carries a FLOAT as the shortest decimal that names it, and a JSON
/// number binds as a DOUBLE: compared bare, the key is widened and 1.1 the
/// FLOAT is not 1.1 the DOUBLE, so the statement names no row. Cast to FLOAT,
/// the param is the key. The UPDATE, the DELETE and the duplicate's INSERT
/// each name exactly one row, in the session transaction a commit runs in.
#[test]
#[ignore]
fn a_float_key_names_its_row_through_a_cast() {
    use serde_json::json;
    let Some(conn) = scratch() else { return };
    let sid = harbor_client::session_new(&conn).expect("session");
    let run = |sql: &str, params: Option<Vec<serde_json::Value>>| {
        harbor_client::exec(&conn, sql, params, Some(&sid)).expect(sql)
    };
    run("DROP TABLE IF EXISTS _dt_floatkey_probe", None);
    run("CREATE TEMP TABLE _dt_floatkey_probe(k FLOAT PRIMARY KEY, name VARCHAR, f FLOAT)", None);
    run("INSERT INTO _dt_floatkey_probe VALUES (0.1, 'a', 0.1), (1.1, 'b', 1.1), (0.5, 'c', 0.5)", None);

    // The keys as the grid fetches them: the identity a statement binds.
    let fetched = run("SELECT k FROM _dt_floatkey_probe ORDER BY name", None);
    println!(
        "column type {:?}, keys on the wire {:?}",
        fetched.columns[0].duckdb_type, fetched.rows
    );
    assert_eq!(fetched.columns[0].duckdb_type, "FLOAT");
    assert_eq!(fetched.rows, vec![vec![json!(0.1)], vec![json!(1.1)], vec![json!(0.5)]]);

    run("BEGIN", None);
    for key in fetched.rows.iter().map(|r| r[0].clone()) {
        let bare = run(
            "SELECT count(*) FROM _dt_floatkey_probe WHERE \"k\" = ?",
            Some(vec![key.clone()]),
        );
        let hit = run(
            "UPDATE _dt_floatkey_probe SET \"name\" = ?, \"f\" = ? WHERE \"k\" = ?::FLOAT",
            Some(vec![json!("hit"), json!(2.2), key.clone()]),
        );
        let copied = run(
            "INSERT INTO _dt_floatkey_probe (\"k\", \"name\", \"f\") SELECT ?, \"name\", \"f\" \
             FROM _dt_floatkey_probe WHERE \"k\" = ?::FLOAT RETURNING k, f",
            Some(vec![json!(key.as_f64().unwrap() + 10.0), key.clone()]),
        );
        println!(
            "key {key}: bare `?` matches {}, `?::FLOAT` updates {}, the duplicate returns {:?}",
            bare.rows[0][0], hit.rows[0][0], copied.rows
        );
        // 0.5 is the same number in both widths; 0.1 and 1.1 are not.
        let exact = key == json!(0.5);
        assert_eq!(bare.rows[0][0].as_u64(), Some(exact as u64), "{key} compared as a DOUBLE");
        assert_eq!(hit.rows[0][0].as_u64(), Some(1), "{key}: one row, exactly");
        assert_eq!(copied.rows.len(), 1, "{key}: the duplicate finds its source");
        let gone = run(
            "DELETE FROM _dt_floatkey_probe WHERE \"k\" = ?::FLOAT",
            Some(vec![key.clone()]),
        );
        assert_eq!(gone.rows[0][0].as_u64(), Some(1), "{key}: one row deleted");
    }
    run("COMMIT", None);

    // A FLOAT value needs no cast in the SET or VALUES list: assignment
    // rounds the DOUBLE to the FLOAT the cast would have made.
    let stored = run("SELECT k, f, f = 2.2::FLOAT FROM _dt_floatkey_probe ORDER BY k", None);
    println!("the copies, keyed and set through a bare `?`: {:?}", stored.rows);
    assert_eq!(
        stored.rows,
        vec![
            vec![json!(10.1), json!(2.2), json!(true)],
            vec![json!(10.5), json!(2.2), json!(true)],
            vec![json!(11.1), json!(2.2), json!(true)],
        ]
    );
    run("DROP TABLE _dt_floatkey_probe", None);
}

/// Why DuckTable's `parse_value` refuses typed text for a container that
/// holds a VARIANT, a JSON or a BLOB. Such a cell is bound as the container's
/// displayed text through a bare `?`, and the engine's cast of that text
/// never reaches the inner value: the statement succeeds, and what is stored
/// is not what was shown.
#[test]
#[ignore]
fn a_container_of_documents_or_blobs_is_corrupted_by_its_own_text() {
    use serde_json::json;
    let Some(conn) = scratch() else { return };
    let sid = harbor_client::session_new(&conn).expect("session");
    let run = |sql: &str, params: Option<Vec<serde_json::Value>>| {
        harbor_client::exec(&conn, sql, params, Some(&sid)).expect(sql)
    };
    run("DROP TABLE IF EXISTS _dt_container_probe", None);
    run(
        "CREATE TEMP TABLE _dt_container_probe(id INTEGER PRIMARY KEY, bl BLOB[], fixed BLOB[2], \
         docs VARIANT[], js JSON[], s STRUCT(v VARIANT, n INTEGER), m MAP(VARCHAR, BLOB))",
        None,
    );
    run(
        "INSERT INTO _dt_container_probe VALUES (1, ['hi'::BLOB], ['hi'::BLOB, '\\xFF'::BLOB], \
         [{'a': 1}::VARIANT], ['{\"a\":1}'::JSON], {'v': {'a': 1}::VARIANT, 'n': 1}, MAP {'k': 'hi'::BLOB})",
        None,
    );
    let shown = run("SELECT bl, fixed, docs, js, s, m FROM _dt_container_probe", None);
    let columns: Vec<&str> = shown.columns.iter().map(|c| c.duckdb_type.as_str()).collect();
    println!("types: {columns:?}");
    assert_eq!(
        columns,
        ["BLOB[]", "BLOB[2]", "VARIANT[]", "JSON[]", "STRUCT(v VARIANT, n INTEGER)", "MAP(VARCHAR, BLOB)"]
    );

    // The text the grid shows for each cell, bound back the way a typed edit
    // of a nested type is: bare, for the engine to cast. Each column is
    // probed in its own transaction and rolled back.
    let text = |v: &serde_json::Value| match v {
        serde_json::Value::String(s) => json!(s),
        other => json!(other.to_string()),
    };
    // A MAP's pairs are not text the engine casts to a MAP, so its cell is
    // retyped the way the engine writes one, with the same base64.
    for (ix, (col, inner, typed)) in [
        ("bl", "bl[1]::VARCHAR", None),
        ("fixed", "fixed[1]::VARCHAR", None),
        ("docs", "variant_typeof(docs[1])", None),
        ("js", "json_type(js[1])", None),
        ("s", "variant_typeof(s.v)", None),
        ("m", "m['k']::VARCHAR", Some("{k=aGk=}")),
    ]
    .into_iter()
    .enumerate()
    {
        let read = format!("SELECT {inner} FROM _dt_container_probe");
        let before = run(&read, None).rows.remove(0).remove(0);
        let bound = typed.map(|t| json!(t)).unwrap_or_else(|| text(&shown.rows[0][ix]));
        run("BEGIN", None);
        let sql = format!("UPDATE _dt_container_probe SET \"{col}\" = ? WHERE \"id\" = ?");
        let hit = harbor_client::exec(&conn, &sql, Some(vec![bound.clone(), json!(1)]), Some(&sid));
        let hit = hit.unwrap_or_else(|e| panic!("{col} = {bound}: {e}"));
        assert_eq!(hit.rows[0][0].as_u64(), Some(1), "{col}: the statement succeeds");
        let after = run(&read, None).rows.remove(0).remove(0);
        println!("{col} = {bound}: {inner} was {before}, is {after}");
        assert_ne!(before, after, "{col}: the same text is not the same value");
        run("ROLLBACK", None);
    }
    run("DROP TABLE _dt_container_probe", None);
}

/// The JSON a document cell takes (ducktable's `check_json`): strict, and at
/// most 100 levels deep. A 100-level document binds through `?::JSON` into a
/// VARIANT and a JSON column and reads back whole. The engine's JSON also
/// reads NaN, which JSON does not have; the cell that comes back is then not
/// JSON, which is why the editor refuses it.
#[test]
#[ignore]
fn a_document_cell_takes_strict_json_a_hundred_levels_deep() {
    use serde_json::json;
    let Some(conn) = scratch() else { return };
    let sid = harbor_client::session_new(&conn).expect("session");
    let run = |sql: &str, params: Option<Vec<serde_json::Value>>| {
        harbor_client::exec(&conn, sql, params, Some(&sid)).expect(sql)
    };
    run("DROP TABLE IF EXISTS _dt_depth_probe", None);
    run("CREATE TEMP TABLE _dt_depth_probe(id INTEGER PRIMARY KEY, doc VARIANT, j JSON)", None);
    let deep = format!("{}1{}", "[{\"a\":".repeat(50), "}]".repeat(50));
    run("BEGIN", None);
    let made = run(
        "INSERT INTO _dt_depth_probe (\"id\", \"doc\", \"j\") VALUES (?, ?::JSON, ?::JSON) RETURNING id",
        Some(vec![json!(1), json!(deep), json!(deep)]),
    );
    assert_eq!(made.rows.len(), 1);
    let hit = run(
        "UPDATE _dt_depth_probe SET \"doc\" = ?::JSON, \"j\" = ?::JSON WHERE \"id\" = ?",
        Some(vec![json!(deep), json!(deep), json!(1)]),
    );
    assert_eq!(hit.rows[0][0].as_u64(), Some(1));
    run("COMMIT", None);
    let back = run("SELECT doc, j, variant_typeof(doc) FROM _dt_depth_probe", None);
    // A document cell reaches this client as its JSON text.
    let document = |cell: &serde_json::Value| -> Result<serde_json::Value, serde_json::Error> {
        serde_json::from_str(cell.as_str().expect("JSON text"))
    };
    let parsed: serde_json::Value = serde_json::from_str(&deep).unwrap();
    println!("100 levels: variant_typeof {}, {} bytes back", back.rows[0][2], back.rows[0][0].as_str().unwrap().len());
    assert_eq!(back.rows[0][2], json!("ARRAY(1)"));
    assert_eq!(document(&back.rows[0][0]).unwrap(), parsed, "the VARIANT reads back whole");
    assert_eq!(document(&back.rows[0][1]).unwrap(), parsed, "and so does the JSON column");

    // The same text into both: a JSON column keeps it, character for
    // character; a VARIANT keeps its values.
    let typed = "{ \"p\": 100.00,  \"a\": 1, \"a\": 2 }";
    let kept = run(
        "INSERT INTO _dt_depth_probe (\"id\", \"doc\", \"j\") VALUES (?, ?::JSON, ?::JSON) \
         RETURNING doc, j, variant_typeof(doc.p)",
        Some(vec![json!(3), json!(typed), json!(typed)]),
    );
    println!(
        "typed {typed:?}: the VARIANT reads back {} with p a {}, the JSON column {}",
        kept.rows[0][0], kept.rows[0][2], kept.rows[0][1]
    );
    assert_eq!(kept.rows[0][1], json!(typed), "a JSON column stores the text as typed");
    assert_eq!(kept.rows[0][2], json!("DOUBLE"), "and a JSON decimal is a DOUBLE in a VARIANT");
    assert_eq!(kept.rows[0][0], json!("{\"p\":100.0,\"a\":2}"), "a VARIANT reads back compact");

    let nan = run(
        "INSERT INTO _dt_depth_probe (\"id\", \"doc\") VALUES (?, ?::JSON) RETURNING doc, variant_typeof(doc.x)",
        Some(vec![json!(2), json!("{\"x\": NaN}")]),
    );
    println!("NaN through ?::JSON: {:?}", nan.rows[0]);
    assert_eq!(nan.rows[0][1], json!("DOUBLE"), "the engine takes NaN in a document");
    assert_eq!(
        document(&nan.rows[0][0]).unwrap(),
        json!({"x": "NaN"}),
        "and it comes back as JSON, the NaN a string as in a DOUBLE column"
    );
    run("DROP TABLE _dt_depth_probe", None);
}

/// Why DuckTable's `parse_value` holds a FLOAT cell to a FLOAT's range. A
/// typed number is bound as a JSON number, a DOUBLE, and assigned to the
/// column: the engine rounds it to the nearest FLOAT, and refuses one that
/// rounds past the largest — at commit, where the whole transaction goes
/// with it. A DOUBLE column takes the same numbers.
#[test]
#[ignore]
fn a_float_cell_holds_what_rounds_to_a_float() {
    use serde_json::json;
    let Some(conn) = scratch() else { return };
    let sid = harbor_client::session_new(&conn).expect("session");
    let run = |sql: &str, params: Option<Vec<serde_json::Value>>| {
        harbor_client::exec(&conn, sql, params, Some(&sid)).expect(sql)
    };
    run("DROP TABLE IF EXISTS _dt_floatrange_probe", None);
    run("CREATE TEMP TABLE _dt_floatrange_probe(k INTEGER, f FLOAT, d DOUBLE)", None);
    let typed = run("SELECT f, d FROM _dt_floatrange_probe", None);
    println!("column types {:?}", typed.columns.iter().map(|c| &c.duckdb_type).collect::<Vec<_>>());
    assert_eq!(typed.columns[0].duckdb_type, "FLOAT");

    // The largest FLOAT, and a DOUBLE past it that still rounds to it.
    for text in ["3.4028235e38", "-3.4028235e38", "3.40282356e38", "1e-50"] {
        let v: f64 = text.parse().unwrap();
        let kept = run(
            "INSERT INTO _dt_floatrange_probe (k, f) VALUES (?, ?) RETURNING f",
            Some(vec![json!(1), json!(v)]),
        );
        println!("{text} into FLOAT: {}", kept.rows[0][0]);
        // The wire names the FLOAT by its shortest decimal.
        assert_eq!(kept.rows[0][0].as_f64().map(|k| k as f32), Some(v as f32), "{text}");
        assert!((v as f32).is_finite(), "{text}: the editor's test agrees");
    }
    // Halfway to the next power of two, and everything beyond it.
    for text in ["3.4028235677973366e38", "3.4028236e38", "3.5e38", "-3.5e38", "1e39"] {
        let v: f64 = text.parse().unwrap();
        let refused = harbor_client::exec(
            &conn,
            "INSERT INTO _dt_floatrange_probe (k, f) VALUES (?, ?)",
            Some(vec![json!(2), json!(v)]),
            Some(&sid),
        );
        println!("{text} into FLOAT: {:?}", refused.as_ref().err());
        assert!(refused.is_err(), "{text} is past a FLOAT");
        assert!(!(v as f32).is_finite(), "{text}: the editor's test agrees");
        let kept = run(
            "INSERT INTO _dt_floatrange_probe (k, d) VALUES (?, ?) RETURNING d",
            Some(vec![json!(3), json!(v)]),
        );
        assert_eq!(kept.rows[0][0].as_f64(), Some(v), "{text} is a DOUBLE");
    }
    run("DROP TABLE _dt_floatrange_probe", None);
}

/// Why text typed into a BLOB cell is never SQL NULL (ducktable's
/// `parse_value`). `null`, in any case, is four base64 characters, and
/// through the BLOB placeholder each spelling is three bytes a cell can hold
/// and show. NULL reaches the column as a NULL param, which is what ⌃⇧N and
/// Delete bind.
#[test]
#[ignore]
fn null_typed_into_a_blob_cell_is_three_bytes() {
    use serde_json::json;
    let Some(conn) = scratch() else { return };
    let sid = harbor_client::session_new(&conn).expect("session");
    let run = |sql: &str, params: Option<Vec<serde_json::Value>>| {
        harbor_client::exec(&conn, sql, params, Some(&sid)).expect(sql)
    };
    run("DROP TABLE IF EXISTS _dt_blobnull_probe", None);
    run("CREATE TEMP TABLE _dt_blobnull_probe(k INTEGER, b BLOB)", None);
    for (text, bytes) in [("null", "9EE965"), ("NULL", "3542CB"), ("Null", "36E965")] {
        let kept = run(
            "INSERT INTO _dt_blobnull_probe (k, b) VALUES (?, from_base64(?::VARCHAR)) \
             RETURNING hex(b), b, b IS NULL",
            Some(vec![json!(1), json!(text)]),
        );
        println!("{text:?} through from_base64(?::VARCHAR): {:?}", kept.rows[0]);
        assert_eq!(kept.rows[0], vec![json!(bytes), json!(text), json!(false)]);
    }
    let null = run(
        "INSERT INTO _dt_blobnull_probe (k, b) VALUES (?, from_base64(?::VARCHAR)) RETURNING b IS NULL",
        Some(vec![json!(2), serde_json::Value::Null]),
    );
    println!("a NULL param through it: b IS NULL = {}", null.rows[0][0]);
    assert_eq!(null.rows[0][0], json!(true));
    run("DROP TABLE _dt_blobnull_probe", None);
}

/// Which types keep the whitespace around their text (ducktable's
/// `edits.rs`, `keeps_whitespace`). Padded text bound into a number, a date,
/// a list or a VARIANT is the value the bare text names, and a UUID, a BIT
/// and base64 refuse it; so padding alone is never a change to such a cell.
/// A VARCHAR, a JSON column and an ENUM store it: there it is another value.
#[test]
#[ignore]
fn padding_is_part_of_a_value_only_for_text_json_and_enum() {
    use serde_json::json;
    let Some(conn) = scratch() else { return };
    let sid = harbor_client::session_new(&conn).expect("session");
    let run = |sql: &str, params: Option<Vec<serde_json::Value>>| {
        harbor_client::exec(&conn, sql, params, Some(&sid)).expect(sql)
    };
    run("DROP TABLE IF EXISTS _dt_padding_probe", None);
    for (ty, bare, placeholder) in [
        ("INTEGER", "5", "?"),
        ("HUGEINT", "170141183460469231731687303715884105727", "?"),
        ("DOUBLE", "1.5", "?"),
        ("DECIMAL(10,2)", "1.50", "?"),
        ("DATE", "2024-02-29", "?"),
        ("TIMESTAMP", "2024-02-29 01:02:03", "?"),
        ("TIME", "01:02:03", "?"),
        ("INTERVAL", "3 days", "?"),
        ("INTEGER[]", "[1, 2]", "?"),
        ("STRUCT(a INTEGER)", "{'a': 1}", "?"),
        ("VARIANT", "{\"a\":1}", "?::JSON"),
    ] {
        run(&format!("CREATE OR REPLACE TEMP TABLE _dt_padding_probe(k INTEGER, v {ty})"), None);
        let insert = format!("INSERT INTO _dt_padding_probe VALUES (?, {placeholder})");
        run(&insert, Some(vec![json!(1), json!(bare)]));
        run(&insert, Some(vec![json!(2), json!(format!(" {bare} "))]));
        let same = run("SELECT count(DISTINCT v::VARCHAR), min(v::VARCHAR) FROM _dt_padding_probe", None);
        println!("{ty}: padded and bare are {} value(s), {}", same.rows[0][0], same.rows[0][1]);
        assert_eq!(same.rows[0][0].as_u64(), Some(1), "{ty}");
    }
    for (ty, bare, placeholder) in [
        ("UUID", "6f9619ff-8b86-d011-b42d-00c04fc964ff", "?"),
        ("BIT", "101", "?"),
        ("BLOB", "qg==", "from_base64(?::VARCHAR)"),
    ] {
        run(&format!("CREATE OR REPLACE TEMP TABLE _dt_padding_probe(k INTEGER, v {ty})"), None);
        let insert = format!("INSERT INTO _dt_padding_probe VALUES (?, {placeholder})");
        run(&insert, Some(vec![json!(1), json!(bare)]));
        let refused =
            harbor_client::exec(&conn, &insert, Some(vec![json!(2), json!(format!(" {bare} "))]), Some(&sid));
        println!("{ty}: padded text is refused: {:?}", refused.as_ref().err().map(|e| e.lines().next().unwrap_or("").to_string()));
        assert!(refused.is_err(), "{ty}");
    }
    for (ty, bare, placeholder) in [
        ("VARCHAR", "5", "?"),
        ("JSON", "{\"a\":1}", "?::JSON"),
        ("ENUM('a', ' a ')", "a", "?"),
    ] {
        run(&format!("CREATE OR REPLACE TEMP TABLE _dt_padding_probe(k INTEGER, v {ty})"), None);
        let insert = format!("INSERT INTO _dt_padding_probe VALUES (?, {placeholder})");
        run(&insert, Some(vec![json!(1), json!(bare)]));
        run(&insert, Some(vec![json!(2), json!(format!(" {bare} "))]));
        let kept = run("SELECT v::VARCHAR FROM _dt_padding_probe ORDER BY k", None);
        println!("{ty}: stored {:?}", kept.rows);
        assert_eq!(kept.rows, vec![vec![json!(bare)], vec![json!(format!(" {bare} "))]], "{ty}");
    }
    run("DROP TABLE _dt_padding_probe", None);
}

/// How a grid notices a table altered elsewhere (ducktable's `grid.rs`,
/// `same_columns`). One client pages a table; another alters a column's type
/// and adds a column; the first client's next page of the same SQL carries
/// the new names and types, which is all the signal there is. The stale type
/// matters: base64 text bound through the placeholder of the VARCHAR the
/// column was is stored in the BLOB it became as the characters themselves.
#[test]
#[ignore]
fn a_table_altered_elsewhere_shows_in_the_next_page() {
    use serde_json::json;
    let Some(grid) = scratch() else { return };
    let other = scratch().expect("connect again");
    let elsewhere = harbor_client::session_new(&other).expect("session");
    let alter = |sql: &str| harbor_client::exec(&other, sql, None, Some(&elsewhere)).expect(sql);
    let shape = |r: &harbor_client::QueryResult| -> Vec<(String, String)> {
        r.columns.iter().map(|c| (c.name.clone().unwrap_or_default(), c.duckdb_type.clone())).collect()
    };
    alter("DROP TABLE IF EXISTS main._dt_reshape_probe");
    alter("CREATE TABLE main._dt_reshape_probe(id INTEGER PRIMARY KEY, payload VARCHAR)");
    alter("INSERT INTO main._dt_reshape_probe VALUES (1, 'qg==')");

    let page = "SELECT * FROM \"main\".\"_dt_reshape_probe\" LIMIT 500 OFFSET 0";
    let born = harbor_client::query(&grid, page).expect("first page");
    println!("the grid's birth: {:?}", shape(&born));
    assert_eq!(shape(&born), vec![("id".into(), "INTEGER".into()), ("payload".into(), "VARCHAR".into())]);

    alter("ALTER TABLE main._dt_reshape_probe ALTER payload TYPE BLOB");
    alter("ALTER TABLE main._dt_reshape_probe ADD COLUMN note VARCHAR DEFAULT 'n'");
    let next = harbor_client::query(&grid, page).expect("next page");
    println!("the next page:    {:?}", shape(&next));
    assert_eq!(
        shape(&next),
        vec![("id".into(), "INTEGER".into()), ("payload".into(), "BLOB".into()), ("note".into(), "VARCHAR".into())]
    );
    assert_ne!(shape(&born), shape(&next));

    // What the stale VARCHAR placeholder does to the BLOB, and what the
    // BLOB's own placeholder does.
    for (id, placeholder) in [(2, "?"), (3, "from_base64(?::VARCHAR)")] {
        let sql = format!("INSERT INTO main._dt_reshape_probe (id, payload) VALUES (?, {placeholder}) RETURNING hex(payload)");
        let kept = harbor_client::exec(&grid, &sql, Some(vec![json!(id), json!("qrs=")]), None).expect("insert");
        println!("'qrs=' through `{placeholder}`: bytes {}", kept.rows[0][0]);
    }
    let stored = harbor_client::query(&grid, "SELECT hex(payload) FROM main._dt_reshape_probe WHERE id > 1 ORDER BY id")
        .expect("read back");
    assert_eq!(stored.rows, vec![vec![json!("7172733D")], vec![json!("AABB")]]);
    alter("DROP TABLE main._dt_reshape_probe");
    harbor_client::session_release(&other, &elsewhere);
}

/// Why a list or struct cell refuses a backslash escape other than `\"` and
/// `\\` (ducktable's `lossy_escape`). The grid shows a newline inside a list
/// as `\n`, the JSON spelling; bound back bare, the engine's cast reads a
/// backslash inside quotes as "take the next character as it is" and stores
/// the letter. Outside quotes it keeps the backslash.
#[test]
#[ignore]
fn a_container_cast_reads_an_escape_as_plain_characters() {
    use serde_json::json;
    let Some(conn) = scratch() else { return };
    let sid = harbor_client::session_new(&conn).expect("session");
    let run = |sql: &str, params: Option<Vec<serde_json::Value>>| {
        harbor_client::exec(&conn, sql, params, Some(&sid)).expect(sql)
    };
    run("DROP TABLE IF EXISTS _dt_escape_probe", None);
    run(
        "CREATE TEMP TABLE _dt_escape_probe(id INTEGER PRIMARY KEY, l VARCHAR[], s STRUCT(b VARCHAR))",
        None,
    );
    run(
        "INSERT INTO _dt_escape_probe VALUES (1, ['line' || chr(10) || 'break', 'x'], {'b': 'tab' || chr(9) || 'here'})",
        None,
    );
    let shown = run("SELECT l, s FROM _dt_escape_probe", None);
    // What the cell shows: serde's text of the wire value.
    let list = shown.rows[0][0].to_string();
    let record = shown.rows[0][1].to_string();
    println!("the cells show {list} and {record}");
    assert_eq!(list, r#"["line\nbreak","x"]"#);
    assert_eq!(record, r#"{"b":"tab\there"}"#);

    // That text typed back, with only `x` changed to `y`: the statement
    // succeeds and the first element has lost its newline.
    run("BEGIN", None);
    let hit = run(
        "UPDATE _dt_escape_probe SET l = ?, s = ? WHERE id = ?",
        Some(vec![json!(list.replace("\"x\"", "\"y\"")), json!(record), json!(1)]),
    );
    assert_eq!(hit.rows[0][0].as_u64(), Some(1));
    let after = run("SELECT l[1], l[2], s.b FROM _dt_escape_probe", None);
    println!("stored: {:?}", after.rows[0]);
    assert_eq!(after.rows[0], vec![json!("linenbreak"), json!("y"), json!("tabthere")]);
    run("ROLLBACK", None);

    // Every escape but the quote and the backslash loses its meaning.
    for (typed, stored) in [
        (r#"["a\rb"]"#, "arb"),
        (r#"["a\bb"]"#, "abb"),
        (r#"["a\fb"]"#, "afb"),
        (r#"["a\u0001b"]"#, "au0001b"),
        (r#"["C:\dir"]"#, "C:dir"),
        (r"['a\nb']", "anb"),
        // The two the cast reads back as the character they name.
        (r#"["q\"uote"]"#, "q\"uote"),
        (r#"["back\\slash"]"#, "back\\slash"),
        // Outside quotes a backslash is kept, except before a quote.
        (r"[C:\dir]", r"C:\dir"),
        (r"[a\nb]", r"a\nb"),
        (r#"[a\"b]"#, "a\"b"),
    ] {
        let read = run("SELECT (?::VARCHAR[])[1]", Some(vec![json!(typed)]));
        println!("{typed} -> {}", read.rows[0][0]);
        assert_eq!(read.rows[0][0], json!(stored), "{typed}");
    }
    run("DROP TABLE _dt_escape_probe", None);
    harbor_client::session_release(&conn, &sid);
}

/// Why a UNION cell refuses typed text. The text is bound as a VARCHAR, and
/// the engine stores a VARCHAR under the member of that type whatever the
/// cell showed; with no VARCHAR member the cast fails.
#[test]
#[ignore]
fn a_union_stores_typed_text_under_its_varchar_member() {
    use serde_json::json;
    let Some(conn) = scratch() else { return };
    let sid = harbor_client::session_new(&conn).expect("session");
    let run = |sql: &str, params: Option<Vec<serde_json::Value>>| {
        harbor_client::exec(&conn, sql, params, Some(&sid)).expect(sql)
    };
    run("DROP TABLE IF EXISTS _dt_union_probe", None);
    run(
        "CREATE TEMP TABLE _dt_union_probe(id INTEGER PRIMARY KEY, u UNION(n INTEGER, s VARCHAR), d UNION(n INTEGER, f DOUBLE))",
        None,
    );
    run("INSERT INTO _dt_union_probe VALUES (1, union_value(n := 7), union_value(n := 7))", None);
    let shown = run("SELECT u FROM _dt_union_probe", None);
    let cell = shown.rows[0][0].to_string();
    println!("the cell shows {cell}");
    assert_eq!(cell, r#"{"tag":"n","value":7}"#);

    for typed in ["8", cell.as_str()] {
        run("BEGIN", None);
        run("UPDATE _dt_union_probe SET u = ? WHERE id = ?", Some(vec![json!(typed), json!(1)]));
        let after = run("SELECT union_tag(u)::VARCHAR, u.s FROM _dt_union_probe", None);
        println!("{typed} -> {:?}", after.rows[0]);
        assert_eq!(after.rows[0], vec![json!("s"), json!(typed)], "{typed}");
        run("ROLLBACK", None);
    }
    // No VARCHAR member: the statement fails, which at ⌘S is the whole commit.
    run("BEGIN", None);
    let refused = harbor_client::exec(
        &conn,
        "UPDATE _dt_union_probe SET d = ? WHERE id = ?",
        Some(vec![json!("8"), json!(1)]),
        Some(&sid),
    );
    println!("without a VARCHAR member: {refused:?}", refused = refused.as_ref().err());
    assert!(refused.is_err());
    run("ROLLBACK", None);
    run("DROP TABLE _dt_union_probe", None);
    harbor_client::session_release(&conn, &sid);
}

/// What the engine does with decimal text, which the editor's range check
/// mirrors (ducktable's `decimal_fits`): digits past the scale round half
/// away from zero, the rounded value is held to `width - scale` integer
/// digits, and the digits before an exponent are held to them too.
#[test]
#[ignore]
fn a_decimal_cast_rounds_to_its_scale_and_refuses_past_its_width() {
    use serde_json::json;
    let Some(conn) = scratch() else { return };
    let cast = |text: &str, ty: &str| {
        harbor_client::exec(
            &conn,
            &format!("SELECT (?::VARCHAR)::{ty}"),
            Some(vec![json!(text)]),
            None,
        )
        .map(|r| r.rows[0][0].clone())
    };
    for (text, stored) in [
        ("999.99", "999.99"),
        ("999.994", "999.99"),
        ("-999.994", "-999.99"),
        ("123.456", "123.46"),
        ("0.005", "0.01"),
        ("0.004", "0.00"),
        ("1e2", "100.00"),
        ("100e-2", "1.00"),
        ("0.001e3", "1.00"),
        ("9.99994e2", "999.99"),
        ("1e-1000", "0.00"),
        (".5", "0.50"),
        ("5.", "5.00"),
        ("+1.5", "1.50"),
        ("00012.5", "12.50"),
        ("00999e-1", "99.90"),
    ] {
        assert_eq!(cast(text, "DECIMAL(5,2)"), Ok(json!(stored)), "{text}");
    }
    for text in [
        "12345.6", "1000", "999.995", "-999.995", "999.999", "1e3", "9.99999e2", "0.1e4",
        "99999e-2", "9999e-1", "1000e-1", "1234.5e-1", "1e40",
    ] {
        let refused = cast(text, "DECIMAL(5,2)");
        assert!(refused.is_err(), "{text}: {refused:?}");
    }
    assert_eq!(cast("0.99994", "DECIMAL(4,4)"), Ok(json!("0.9999")));
    assert!(cast("0.99995", "DECIMAL(4,4)").is_err());
    assert!(cast("1", "DECIMAL(4,4)").is_err());
    assert_eq!(cast("999999999999999999.4", "DECIMAL(18,0)"), Ok(json!("999999999999999999")));
    assert!(cast("999999999999999999.5", "DECIMAL(18,0)").is_err());
    assert_eq!(cast("999999999999999.9994", "DECIMAL"), Ok(json!("999999999999999.999")));
    assert!(cast("1000000000000000", "DECIMAL").is_err());
}

/// What the Query view's transactions stand on (ducktable's `query.rs`): a
/// session holds a transaction across requests, other connections do not see
/// it until COMMIT, a statement that fails to parse leaves it as it was, any
/// other error aborts it, a COMMIT of an aborted one is refused and rolled
/// back, and a released session is gone by name.
#[test]
#[ignore]
fn a_session_holds_a_transaction_the_way_the_query_view_expects() {
    use harbor_client::Failure;
    use serde_json::json;
    let Some(conn) = scratch() else { return };
    let alone = |sql: &str| harbor_client::exec_checked(&conn, sql, None, None);
    alone("CREATE OR REPLACE TABLE _dt_txn_probe(id INTEGER PRIMARY KEY, v VARCHAR)").expect("create");
    alone("INSERT INTO _dt_txn_probe VALUES (1, 'a')").expect("insert");

    // Alone, a BEGIN would open a transaction nothing can join: Harbor refuses it.
    let refused = alone("BEGIN").unwrap_err();
    println!("BEGIN alone: {refused}");
    assert!(matches!(&refused, Failure::Refused { code, .. } if code == "sql_error"), "{refused:?}");
    assert!(matches!(alone("ROLLBACK"), Err(Failure::Refused { .. })));

    let session = harbor_client::session_open(&conn).expect("session");
    println!("session: lives {:?}, idles out after {:?}", session.ttl, session.idle);
    assert!(!session.ttl.is_zero() && !session.idle.is_zero());
    let held = |sql: &str| harbor_client::exec_checked(&conn, sql, None, Some(&session.id));
    let value = |r: Result<harbor_client::QueryResult, Failure>| r.expect("select").rows[0][0].clone();

    held("BEGIN").expect("BEGIN");
    held("UPDATE _dt_txn_probe SET v = 'b' WHERE id = 1").expect("update");
    assert_eq!(value(held("SELECT v FROM _dt_txn_probe")), json!("b"), "the session sees its own write");
    assert_eq!(value(alone("SELECT v FROM _dt_txn_probe")), json!("a"), "nobody else does");

    // A statement that does not parse never ran: the transaction is as it was.
    let parse = held("COMMIT foo").unwrap_err();
    println!("COMMIT foo: {parse}");
    assert!(matches!(&parse, Failure::Refused { message, .. } if message.starts_with("Parser Error")));
    assert_eq!(value(held("SELECT v FROM _dt_txn_probe")), json!("b"));
    held("ROLLBACK").expect("ROLLBACK");
    assert_eq!(value(alone("SELECT v FROM _dt_txn_probe")), json!("a"), "rolled back");

    // Any other error aborts it, and a second BEGIN is such an error.
    held("BEGIN").expect("BEGIN");
    held("UPDATE _dt_txn_probe SET v = 'c' WHERE id = 1").expect("update");
    let again = held("BEGIN").unwrap_err();
    println!("BEGIN inside one: {again}");
    let aborted = held("SELECT 1").unwrap_err();
    println!("then SELECT 1: {aborted}");
    assert!(aborted.to_string().contains("aborted"));
    // Harbor refuses a COMMIT of an aborted transaction and rolls it back.
    let rolled = held("COMMIT").unwrap_err();
    println!("COMMIT of it: {rolled}");
    assert!(
        matches!(&rolled, Failure::Refused { code, message } if code == "sql_error" && message.contains("rolled back")),
        "{rolled:?}"
    );
    assert!(!rolled.session_gone());
    assert_eq!(value(alone("SELECT v FROM _dt_txn_probe")), json!("a"), "nothing landed");
    assert!(matches!(held("ROLLBACK"), Err(Failure::Refused { .. })), "and the transaction is over");

    // A COMMIT the engine refuses ends the transaction rolled back.
    let other = harbor_client::session_open(&conn).expect("second session");
    let rival = |sql: &str| harbor_client::exec_checked(&conn, sql, None, Some(&other.id));
    held("BEGIN").expect("BEGIN");
    held("INSERT INTO _dt_txn_probe VALUES (50, 'mine')").expect("insert");
    rival("BEGIN").expect("BEGIN");
    rival("INSERT INTO _dt_txn_probe VALUES (50, 'theirs')").expect("insert");
    rival("COMMIT").expect("the first COMMIT lands");
    let lost = held("COMMIT").unwrap_err();
    println!("the second COMMIT: {lost}");
    assert!(matches!(&lost, Failure::Refused { code, .. } if code == "sql_error"));
    assert!(!lost.session_gone());
    assert!(matches!(held("ROLLBACK"), Err(Failure::Refused { .. })), "no transaction is left");
    assert_eq!(value(alone("SELECT v FROM _dt_txn_probe WHERE id = 50")), json!("theirs"));
    harbor_client::session_release(&conn, &other.id);

    // Released with a transaction open: rolled back, and gone by name.
    held("BEGIN").expect("BEGIN");
    held("UPDATE _dt_txn_probe SET v = 'd' WHERE id = 1").expect("update");
    harbor_client::session_release(&conn, &session.id);
    let gone = held("SELECT 1").unwrap_err();
    println!("after release: {gone}");
    assert!(gone.session_gone());
    assert_eq!(value(alone("SELECT v FROM _dt_txn_probe WHERE id = 1")), json!("a"));
    alone("DROP TABLE _dt_txn_probe").expect("drop");
}

/// A session left idle is reclaimed by Harbor with its transaction rolled
/// back, and one renewed inside its idle window is kept: the Query view's
/// keepalive. A renewal runs nothing and leaves the session's ceiling where
/// it was. Takes as long as the idle timeout the server grants, plus a few
/// seconds.
#[test]
#[ignore]
fn a_session_touched_in_time_outlives_its_idle_timeout() {
    use serde_json::json;
    let Some(conn) = scratch() else { return };
    let alone = |sql: &str| harbor_client::exec_checked(&conn, sql, None, None);
    alone("CREATE OR REPLACE TABLE _dt_idle_probe(v VARCHAR)").expect("create");
    alone("INSERT INTO _dt_idle_probe VALUES ('a')").expect("insert");

    let kept = harbor_client::session_open(&conn).expect("session");
    let left = harbor_client::session_open(&conn).expect("session");
    let on = |id: &str, sql: &str| harbor_client::exec_checked(&conn, sql, None, Some(id));
    on(&kept.id, "BEGIN").expect("BEGIN");
    on(&left.id, "BEGIN").expect("BEGIN");
    on(&kept.id, "UPDATE _dt_idle_probe SET v = 'kept'").expect("update");

    // What Harbor's list of sessions says of the kept one.
    let listed = |field: &str| -> u64 {
        let r = harbor_client::http::request(
            conn.transport().unwrap(),
            &wire::endpoint::SESSIONS,
            None,
            Some(std::time::Duration::from_secs(5)),
        )
        .expect("sessions");
        let list: serde_json::Value = serde_json::from_str(&r.body_string().unwrap()).unwrap();
        let session = list["sessions"].as_array().unwrap().iter().find(|s| s["sessionId"] == kept.id.as_str());
        session.expect("listed")[field].as_u64().unwrap()
    };
    let (statements, ceiling) = (listed("statements"), listed("expiresInMs"));

    // Past the idle timeout, one session renewed at a third of it.
    let until = std::time::Instant::now() + kept.idle + std::time::Duration::from_secs(4);
    while std::time::Instant::now() < until {
        std::thread::sleep(kept.idle / 3);
        assert_eq!(harbor_client::session_renew(&conn, &kept.id), Ok(true), "the renewed session answers");
    }
    assert_eq!(listed("statements"), statements, "a renewal runs nothing");
    assert!(listed("expiresInMs") + kept.idle.as_millis() as u64 <= ceiling, "and leaves the ceiling");
    let reclaimed = on(&left.id, "SELECT 1").unwrap_err();
    println!("the idle session: {reclaimed}");
    assert!(reclaimed.session_gone());
    assert!(harbor_client::session_renew(&conn, &left.id).unwrap_err().session_gone());
    on(&kept.id, "COMMIT").expect("the renewed session still holds its transaction");
    harbor_client::session_release(&conn, &kept.id);
    assert!(harbor_client::session_renew(&conn, &kept.id).unwrap_err().session_gone());
    let read = alone("SELECT v FROM _dt_idle_probe").expect("select");
    assert_eq!(read.rows[0][0], json!("kept"));
    alone("DROP TABLE _dt_idle_probe").expect("drop");
}

/// What `EXPLAIN ANALYZE` does to a transaction statement, which the Query
/// view's reading of a statement's first word mirrors (ducktable's
/// `txn_effect`): it runs it. `EXPLAIN ANALYZE COMMIT` commits, `EXPLAIN
/// ANALYZE ROLLBACK` rolls back, `EXPLAIN ANALYZE BEGIN` opens a transaction,
/// in every spelling of ANALYZE the engine takes; a plain `EXPLAIN` only
/// plans; and a name that merely begins with a keyword is a name.
#[test]
#[ignore]
fn explain_analyze_runs_the_transaction_statement_it_explains() {
    use harbor_client::Failure;
    use serde_json::json;
    let Some(conn) = scratch() else { return };
    let alone = |sql: &str| harbor_client::exec_checked(&conn, sql, None, None);
    alone("CREATE OR REPLACE TABLE _dt_explain_probe(id INTEGER PRIMARY KEY, v VARCHAR)").expect("create");
    alone("INSERT INTO _dt_explain_probe VALUES (1, 'a')").expect("insert");
    let committed = || alone("SELECT v FROM _dt_explain_probe").expect("select").rows[0][0].clone();
    // Run `statement` inside a transaction that has written 'z', and say
    // whether a transaction is still open afterwards and what is committed.
    let inside = |statement: &str| {
        let session = harbor_client::session_open(&conn).expect("session");
        let held = |sql: &str| harbor_client::exec_checked(&conn, sql, None, Some(&session.id));
        held("BEGIN").expect("BEGIN");
        held("UPDATE _dt_explain_probe SET v = 'z'").expect("update");
        let answer = held(statement);
        let open = held("ROLLBACK").is_ok();
        harbor_client::session_release(&conn, &session.id);
        let stored = committed();
        alone("UPDATE _dt_explain_probe SET v = 'a'").expect("reset");
        println!("{statement:?}: {}, transaction {}, committed {stored}",
            if answer.is_ok() { "answered" } else { "refused" },
            if open { "still open" } else { "ended" });
        (answer, open, stored)
    };

    for statement in [
        "EXPLAIN ANALYZE COMMIT", "explain analyze commit", "EXPLAIN ANALYSE COMMIT",
        "EXPLAIN ANALYZE END", "EXPLAIN (ANALYZE) COMMIT", "EXPLAIN (ANALYZE, FORMAT JSON) COMMIT",
        "EXPLAIN (FORMAT JSON, ANALYZE) COMMIT", "EXPLAIN (ANALYZE false) COMMIT",
        "EXPLAIN ANALYZE (FORMAT JSON) COMMIT", "EXPLAIN /* c */ ANALYZE -- d\n COMMIT",
        "COMMIT--x", "-- c\rCOMMIT",
    ] {
        let (answer, open, stored) = inside(statement);
        assert!(answer.is_ok() && !open, "{statement}");
        assert_eq!(stored, json!("z"), "{statement} committed");
    }
    for statement in ["EXPLAIN ANALYZE ROLLBACK", "EXPLAIN ANALYZE ABORT"] {
        let (answer, open, stored) = inside(statement);
        assert!(answer.is_ok() && !open, "{statement}");
        assert_eq!(stored, json!("a"), "{statement} rolled back");
    }
    // A plain EXPLAIN plans and runs nothing.
    for statement in ["EXPLAIN COMMIT", "EXPLAIN (FORMAT JSON) COMMIT"] {
        let (answer, open, stored) = inside(statement);
        assert!(answer.is_ok() && open, "{statement}");
        assert_eq!(stored, json!("a"), "{statement}");
    }
    // A name is not the keyword it begins with, quoted or not: the engine
    // looks for a table, and the transaction stays open, aborted.
    for statement in ["COMMIT_X", "COMMIT1", "COMMIT$x", "COMMITé", "\"COMMIT\"", "(COMMIT)", "EXPLAIN (FORMAT JSON) ANALYZE COMMIT"] {
        let (answer, open, stored) = inside(statement);
        assert!(matches!(&answer, Err(Failure::Refused { message, .. }) if message.starts_with("Catalog Error")), "{statement}: {answer:?}");
        assert!(open, "{statement}");
        assert_eq!(stored, json!("a"), "{statement}");
    }

    // EXPLAIN ANALYZE BEGIN opens a transaction on its session.
    for statement in ["EXPLAIN ANALYZE BEGIN", "EXPLAIN ANALYZE START TRANSACTION", "EXPLAIN (ANALYZE) BEGIN"] {
        let session = harbor_client::session_open(&conn).expect("session");
        let held = |sql: &str| harbor_client::exec_checked(&conn, sql, None, Some(&session.id));
        held(statement).expect(statement);
        held("UPDATE _dt_explain_probe SET v = 'y'").expect("update");
        assert_eq!(committed(), json!("a"), "{statement}: the write is not committed");
        held("ROLLBACK").expect("a transaction was open");
        harbor_client::session_release(&conn, &session.id);
        assert_eq!(committed(), json!("a"), "{statement}");
        println!("{statement:?}: opened a transaction");
    }
    for statement in ["EXPLAIN BEGIN", "\"BEGIN\"", "BEGIN_X"] {
        let session = harbor_client::session_open(&conn).expect("session");
        let held = |sql: &str| harbor_client::exec_checked(&conn, sql, None, Some(&session.id));
        let _ = held(statement);
        assert!(held("ROLLBACK").is_err(), "{statement}: no transaction was opened");
        harbor_client::session_release(&conn, &session.id);
    }
    alone("DROP TABLE _dt_explain_probe").expect("drop");
}

/// Which errors leave a transaction usable, which the Query view's `never_ran`
/// and docs/QUERY.md state: only one the parser raises. After every other
/// class the next statement answers that the transaction is aborted.
#[test]
#[ignore]
fn only_a_parse_error_leaves_a_transaction_usable() {
    use harbor_client::Failure;
    use serde_json::json;
    let Some(conn) = scratch() else { return };
    let alone = |sql: &str| harbor_client::exec_checked(&conn, sql, None, None);
    alone(
        "CREATE OR REPLACE TABLE _dt_abort_probe(id INTEGER PRIMARY KEY, v VARCHAR, \
         n INTEGER NOT NULL DEFAULT 0, CHECK (n >= 0))",
    )
    .expect("create");
    alone("INSERT INTO _dt_abort_probe VALUES (1, 'a', 0)").expect("insert");
    // Run `statement` inside a transaction that has written 'tx'; the class
    // of its error, and whether the next statement still reads that write.
    let after = |statement: &str, params: Option<Vec<serde_json::Value>>| {
        let session = harbor_client::session_open(&conn).expect("session");
        let held = |sql: &str| harbor_client::exec_checked(&conn, sql, None, Some(&session.id));
        held("BEGIN").expect("BEGIN");
        held("UPDATE _dt_abort_probe SET v = 'tx' WHERE id = 1").expect("update");
        let failure = harbor_client::exec_checked(&conn, statement, params, Some(&session.id))
            .expect_err(statement);
        let Failure::Refused { code, message } = failure else { panic!("{statement}: no answer") };
        assert_eq!(code, "sql_error", "{statement}");
        let next = held("SELECT v FROM _dt_abort_probe WHERE id = 1");
        let usable = match &next {
            Ok(read) => read.rows[0][0] == json!("tx"),
            Err(aborted) => {
                assert!(aborted.to_string().contains("Current transaction is aborted"), "{statement}: {aborted}");
                false
            }
        };
        harbor_client::session_release(&conn, &session.id);
        let class = message.split(':').next().unwrap_or_default().to_string();
        println!("{class:<26} {statement:?}: {}", if usable { "usable" } else { "aborted" });
        (class, usable)
    };

    for statement in ["SELEC 1", "COMMIT foo", "SELECT * FROM (\nUPDATE _dt_abort_probe SET v = 'w'\n) LIMIT 5 OFFSET 0"] {
        assert_eq!(after(statement, None), ("Parser Error".to_string(), true), "{statement}");
    }
    for (statement, class) in [
        ("SELECT * FROM _dt_nope", "Catalog Error"),
        ("SELECT _dt_nofunc(1)", "Catalog Error"),
        ("SELECT nocol FROM _dt_abort_probe", "Binder Error"),
        ("SELECT 1 + DATE '2024-01-01' + TRUE", "Binder Error"),
        ("INSERT INTO _dt_abort_probe VALUES (1, 'dup', 0)", "Constraint Error"),
        ("INSERT INTO _dt_abort_probe VALUES (9, 'x', NULL)", "Constraint Error"),
        ("INSERT INTO _dt_abort_probe VALUES (9, 'x', -1)", "Constraint Error"),
        ("SELECT 'abc'::INTEGER", "Conversion Error"),
        ("UPDATE _dt_abort_probe SET n = v::INTEGER WHERE id = 1", "Conversion Error"),
        ("SELECT 2147483647::INTEGER + 1::INTEGER", "Out of Range Error"),
        ("SELECT error('boom')", "Invalid Input Error"),
        ("BEGIN", "TransactionContext Error"),
        ("SELECT * FROM (\nSELECT * FROM _dt_nope\n) LIMIT 5 OFFSET 0", "Catalog Error"),
    ] {
        assert_eq!(after(statement, None), (class.to_string(), false), "{statement}");
    }
    // A parameter the request did not bring.
    assert_eq!(after("SELECT ?::INTEGER", Some(vec![])), ("Invalid Input Error".to_string(), false));
    alone("DROP TABLE _dt_abort_probe").expect("drop");
}

/// Stop reaches the server of one file and no other, though both report the
/// same name: two files with one stem. A second database of the scratch
/// one's file name is started in a directory beside it and stopped by its
/// path; the scratch one keeps answering.
#[test]
#[ignore]
fn a_stop_shuts_down_only_the_server_of_its_own_file() {
    let Some(conn) = scratch() else { return };
    let scratch_db = scratch_db().unwrap();
    let dir = scratch_db.parent().expect("a directory").join("_dt_stop_probe");
    std::fs::create_dir_all(&dir).expect("probe directory");
    let twin = dir.join(scratch_db.file_name().expect("a file name"));

    let other = fleet::connect_path(&twin).expect("the twin starts on demand");
    let (mine, theirs) = (info(&conn).expect("info"), info(&other).expect("info"));
    println!("two servers: {} on {}, {} on {}", mine.name, mine.database, theirs.name, theirs.database);
    assert_ne!(fleet_path(std::path::Path::new(&mine.database)), fleet_path(std::path::Path::new(&theirs.database)));
    harbor_client::query(&other, "SELECT 1").expect("the twin answers");

    fleet::stop(&twin).expect("stop");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while harbor_client::query(&other, "SELECT 1").is_ok() {
        assert!(std::time::Instant::now() < deadline, "the twin was not stopped");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    harbor_client::query(&conn, "SELECT 1").expect("the scratch database was not the one stopped");
    // Stopping what is not running is no error.
    fleet::stop(&twin).expect("stop again");
    drop(other);
    let _ = std::fs::remove_dir_all(dir);
}

/// Two databases with one file name are two rows, and each row connects to
/// its own file: the survey tells them apart by path, says which is which,
/// and `connect_file` reaches the file it is given under the name they share.
/// Needs a `HARBOR_HOME` of its own; no remote is involved, and no SSH.
#[test]
#[ignore]
fn rows_of_one_name_each_connect_to_their_own_file() {
    if !own_fleet() {
        return;
    }
    let Some(conn) = scratch() else { return };
    let scratch_db = scratch_db().unwrap();
    let dir = scratch_db.parent().expect("a directory").join("_dt_rows_probe");
    std::fs::create_dir_all(&dir).expect("probe directory");
    let twin = dir.join(scratch_db.file_name().expect("a file name"));
    let other = fleet::connect_path(&twin).expect("the twin starts on demand");

    let (here, there) = (fleet_path(&scratch_db), fleet_path(&twin));
    let rows = fleet::survey().rows;
    let row_of = |db: &std::path::Path| {
        rows.iter()
            .find(|r| r.path.as_deref().map(fleet_path).as_deref() == Some(db))
            .unwrap_or_else(|| panic!("no row for {}", db.display()))
    };
    let (mine, theirs) = (row_of(&here), row_of(&there));
    println!("rows: {} at {:?}, {} at {:?}", mine.name, mine.path, theirs.name, theirs.path);
    assert!(mine.state.is_live() && theirs.state.is_live());
    if mine.name == theirs.name {
        // One name, two rows: each says which file it is.
        for row in [mine, theirs] {
            let note = row.note.as_deref().unwrap_or_default();
            assert!(note.starts_with("The file "), "{}: {note:?}", row.name);
        }
    }

    // Each row dials its own file, though the name is the other's too.
    for (row, db) in [(mine, &here), (theirs, &there)] {
        let dialed = fleet::connect_file(&row.name, row.path.as_deref().unwrap()).expect("connect_file");
        let serving = info(&dialed).expect("info").database;
        assert_eq!(&fleet_path(std::path::Path::new(&serving)), db, "{} reached {serving}", row.name);
        assert_eq!(dialed.db.as_deref(), Some(db.as_path()));
        assert!(!dialed.summoned, "it was already running");
    }
    // A count never starts a server: a file nothing serves is not joined.
    let absent = dir.join("_dt_absent.duckdb");
    assert!(fleet::join_file(&mine.name, &absent).is_none());
    assert!(!absent.exists(), "and none was created");

    fleet::stop(&twin).expect("stop");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while harbor_client::query(&other, "SELECT 1").is_ok() {
        assert!(std::time::Instant::now() < deadline, "the twin was not stopped");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    harbor_client::query(&conn, "SELECT 1").expect("the scratch database still answers");
    drop(other);
    let _ = std::fs::remove_dir_all(dir);
}

/// What a commit whose answer was lost does before it reads the page
/// (ducktable's `Grid::commit`): it ends the commit's session and waits for
/// it to be over. An idle session is rolled back before Harbor answers the
/// release; one still running a statement is cancelled, and stays in
/// Harbor's list until the statement returns.
#[test]
#[ignore]
fn an_ended_session_is_over_before_the_page_is_read() {
    use harbor_client::{session_end, Ended};
    use serde_json::json;
    let Some(conn) = scratch() else { return };
    let patience = std::time::Duration::from_secs(15);
    let alone = |sql: &str| harbor_client::exec_checked(&conn, sql, None, None);
    alone("CREATE OR REPLACE TABLE _dt_end_probe(v VARCHAR)").expect("create");
    alone("INSERT INTO _dt_end_probe VALUES ('a')").expect("insert");
    let stored = || alone("SELECT v FROM _dt_end_probe").expect("select").rows[0][0].clone();

    // Idle, with a write uncommitted: over at once, and rolled back.
    let idle = harbor_client::session_open(&conn).expect("session");
    let on = |id: &str, sql: &str| harbor_client::exec_checked(&conn, sql, None, Some(id));
    on(&idle.id, "BEGIN").expect("BEGIN");
    on(&idle.id, "UPDATE _dt_end_probe SET v = 'b'").expect("update");
    assert_eq!(session_end(&conn, &idle.id, patience), Ended::Settled);
    assert_eq!(stored(), json!("a"));
    // The same write can be made at once: nothing of the session lingers.
    alone("UPDATE _dt_end_probe SET v = v").expect("no conflict with an ended session");
    // Ending what is already over, or never was, is settled too.
    assert_eq!(session_end(&conn, &idle.id, patience), Ended::Settled);
    assert_eq!(session_end(&conn, "no-such-session", patience), Ended::Settled);

    // Busy with a statement that would run for a long time: the end cancels
    // it and returns once Harbor has let the session go.
    let busy = harbor_client::session_open(&conn).expect("session");
    on(&busy.id, "BEGIN").expect("BEGIN");
    on(&busy.id, "UPDATE _dt_end_probe SET v = 'c'").expect("update");
    let (running, id) = (conn.clone(), busy.id.clone());
    let statement = std::thread::spawn(move || {
        harbor_client::exec_checked(
            &running,
            "SELECT count(*) FROM range(200000000000) t(i) WHERE i % 7 = 3",
            None,
            Some(&id),
        )
    });
    std::thread::sleep(std::time::Duration::from_millis(500));
    let began = std::time::Instant::now();
    let ended = session_end(&conn, &busy.id, patience);
    println!("a busy session ended as {ended:?} in {:?}", began.elapsed());
    assert_eq!(ended, Ended::Settled);
    let answer = statement.join().expect("the statement's thread");
    println!("its statement: {:?}", answer.as_ref().err().map(ToString::to_string));
    assert!(answer.is_err(), "the statement was cancelled");
    assert_eq!(stored(), json!("a"), "and its transaction rolled back");
    assert!(on(&busy.id, "SELECT 1").unwrap_err().session_gone());
    alone("DROP TABLE _dt_end_probe").expect("drop");
}

/// A request that cannot be sent is told apart from one that got no answer
/// (ducktable's `commit_verdict`): only the second leaves a COMMIT in doubt.
#[test]
#[ignore]
fn a_statement_on_a_stopped_server_was_not_sent() {
    use harbor_client::Failure;
    let Some(_conn) = scratch() else { return };
    let scratch_db = scratch_db().unwrap();
    let dir = scratch_db.parent().expect("a directory").join("_dt_unsent_probe");
    std::fs::create_dir_all(&dir).expect("probe directory");
    let twin = dir.join("unsent.duckdb");
    let other = fleet::connect_path(&twin).expect("the twin starts on demand");
    harbor_client::exec_checked(&other, "SELECT 1", None, None).expect("it answers");
    fleet::stop(&twin).expect("stop");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let failure = loop {
        match harbor_client::exec_checked(&other, "COMMIT", None, None) {
            Err(failure @ Failure::Unsent(_)) => break failure,
            // Still draining: it answers, or hangs up mid-answer.
            _ if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(100)),
            other => panic!("the stopped server still answers: {other:?}"),
        }
    };
    println!("on a stopped server: {failure:?}");
    drop(other);
    let _ = std::fs::remove_dir_all(dir);
}
