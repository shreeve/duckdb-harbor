fn main() {
    let conn = harbor_client::fleet::connect("labs").expect("connect");
    for sql in [
        "PRAGMA table_info('main.tests')",
        "PRAGMA table_info('\"main\".\"tests\"')",
        "SELECT sql FROM duckdb_tables() WHERE schema_name='main' AND table_name='tests'",
    ] {
        match harbor_client::query(&conn, sql) {
            Ok(r) => println!("OK  {} cols={:?} first={:?}", sql,
                r.columns.iter().map(|c| c.name.clone().unwrap_or_default()).collect::<Vec<_>>(),
                r.rows.first()),
            Err(e) => println!("ERR {} -> {}", sql, e),
        }
    }
}
