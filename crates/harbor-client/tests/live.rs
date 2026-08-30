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
    assert!(harbor_client::keepalive(&conn));
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
    // A TEMP table lives on the pinned connection and dies with it —
    // the probe never touches the database's real schema.
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
    harbor_client::session_release(&conn, &sid);
    println!("session {sid}: transaction, params, counts — all as the spec assumes");
}
