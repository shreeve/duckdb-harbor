//! Probes against the machine's real fleet. Ignored by default: they need a
//! live Harbor and say nothing in CI. Run explicitly:
//! `cargo test -p harbor-client --test live -- --ignored --nocapture`

use harbor_client::{connect, fleet, info};

#[test]
#[ignore]
fn the_real_fleet_lists_and_answers() {
    let rows = fleet::list();
    println!("fleet:");
    for row in &rows {
        let live = row.transport.as_ref().map(fleet::probe);
        let state = fleet::state_of(row, live);
        println!("  {} {} (summonable: {})", state.label(), row.name, row.summonable());
    }
    assert!(!rows.is_empty(), "no berths known to config or runtime");
}

#[test]
#[ignore]
fn a_live_berth_yields_identity() {
    let live = fleet::list()
        .into_iter()
        .find(|r| r.transport.as_ref().map(fleet::probe).unwrap_or(false));
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
