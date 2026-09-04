//! Probes against the machine's real fleet. Ignored by default: they need a
//! live Harbor and say nothing in CI. Run explicitly:
//! `cargo test -p harbor-client --test live -- --ignored --nocapture`

use harbor_client::{connect, fleet, info};

/// Any berth will do — a name is a service that starts on use, so a stopped
/// configured berth is as connectable as a live one. Live still wins, to
/// avoid churning starts when something is already up.
fn connectable() -> Option<fleet::Survey> {
    let mut rows = fleet::survey().rows;
    rows.sort_by_key(|r| !r.state.is_live());
    rows.into_iter().next()
}

#[test]
#[ignore]
fn the_real_fleet_lists_and_answers() {
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
    assert!(!fleet.rows.is_empty(), "no berths known to config or runtime");
}

#[test]
#[ignore]
fn a_live_berth_yields_identity() {
    let Some(row) = connectable() else {
        println!("no berth to test against; skipping");
        return;
    };
    let conn = connect(&row.name).expect("connect");
    let identity = info(&conn).expect("info");
    println!(
        "{}: duckdb {} harbor {} db {}",
        identity.name, identity.duckdb_version, identity.harbor_version, identity.database
    );
    assert_eq!(identity.name, row.name);
}

#[test]
#[ignore]
fn a_live_berth_answers_sql() {
    let Some(row) = connectable() else {
        println!("no berth to test against; skipping");
        return;
    };
    let conn = connect(&row.name).expect("connect");
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
    let Some(row) = connectable() else {
        println!("no berth to test against; skipping");
        return;
    };
    let conn = connect(&row.name).expect("connect");
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
    let Some(row) = connectable() else {
        println!("no berth to test against; skipping");
        return;
    };
    let conn = connect(&row.name).expect("connect");
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
    let sql = format!("SELECT rowid, * FROM \"{schema}\".\"{name}\" LIMIT 3");
    let result = harbor_client::query(&conn, &sql).expect("rowid probe");
    println!("rowid probe on {schema}.{name}:");
    println!("  columns: {:?}", result.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>());
    for r in &result.rows {
        println!("  {:?}", r.first());
    }
    assert!(result.columns.first().and_then(|c| c.name.as_deref()) == Some("rowid"));
    // And the shape an UPDATE would use: quoted pseudocolumn in WHERE.
    let sql = format!("SELECT count(*) FROM \"{schema}\".\"{name}\" WHERE \"rowid\" = 0");
    let count = harbor_client::query(&conn, &sql).expect("quoted rowid in WHERE");
    println!("  quoted-WHERE count row: {:?}", count.rows.first());
}
