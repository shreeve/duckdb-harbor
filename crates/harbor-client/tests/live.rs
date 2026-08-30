//! Probes against the machine's real fleet. Ignored by default: they need a
//! live Harbor and say nothing in CI. Run explicitly:
//! `cargo test -p harbor-client --test live -- --ignored --nocapture`

use harbor_client::{connect, fleet, info};

#[test]
#[ignore]
fn the_real_fleet_lists_and_answers() {
    let rows = fleet::survey();
    println!("fleet:");
    for row in &rows {
        println!("  {} {}", row.state.label(), row.name);
        if let Some(note) = &row.note {
            println!("    note: {note}");
        }
    }
    assert!(!rows.is_empty(), "no berths known to config or runtime");
}

#[test]
#[ignore]
fn a_live_berth_yields_identity() {
    let live = fleet::survey().into_iter().find(|r| r.state.is_live());
    let Some(row) = live else {
        println!("no live berth to test against; skipping");
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
    let live = fleet::survey().into_iter().find(|r| r.state.is_live());
    let Some(row) = live else {
        println!("no live berth to test against; skipping");
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
