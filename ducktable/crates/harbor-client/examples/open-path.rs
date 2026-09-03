//! Drive the File→Open plumbing exactly as the UI does: connect_path on a
//! bare .duckdb path (join-or-summon), one query through the wire, then
//! stop the summoned server. A summoned server is ephemeral — it would
//! self-retire once its last client left — so this stop is only to skip the
//! idle grace and free the file at once. `cargo run -p harbor-client
//! --example open-path -- <db>`; point HARBOR_BIN/HARBOR_LIBDUCKDB as needed.

use std::path::Path;

fn main() {
    let path = std::env::args().nth(1).expect("usage: open-path <db.duckdb>");
    let conn = harbor_client::fleet::connect_path(Path::new(&path)).expect("connect_path");
    println!("connected: name={:?} summoned={}", conn.name, conn.summoned);
    let result = harbor_client::query(&conn, "SELECT 42 AS ok").expect("query");
    println!("query answered: {:?}", result.rows);
    let info = harbor_client::fleet::info(&conn).expect("info");
    println!("server: {} pid={} db={}", info.name, info.pid, info.database);
    // Leave the harbor as found now, rather than after the idle grace: the
    // ephemeral server we summoned, we stop.
    if conn.summoned {
        harbor_client::http::request(
            &conn.transport,
            &wire::endpoint::SHUTDOWN,
            None,
            Some(std::time::Duration::from_secs(5)),
        )
        .expect("shutdown");
        println!("summoned server stopped");
    }
}
