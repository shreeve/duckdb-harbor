//! Client-side SQL against a berth: text construction (quoting, paging)
//! and result shaping. One home for every query the app hand-writes, so
//! the render files (grid, sidebar, content) never own SQL for another
//! surface. All of these block; callers run them on a background thread.

use crate::util::qident;
use harbor_client::Conn;
use serde_json::Value;

/// Fetch a table's first page; app.rs calls this before it builds the
/// grid (DESIGN.md: fetch first, commit over the old value).
pub(crate) fn first_page(
    conn: &Conn,
    schema: &str,
    name: &str,
    limit: usize,
) -> Result<harbor_client::QueryResult, String> {
    let sql = format!("SELECT * FROM {}.{} LIMIT {}", qident(schema), qident(name), limit);
    harbor_client::query(conn, &sql)
}

/// The table's exact row count, for the status line.
pub(crate) fn total_rows(conn: &Conn, schema: &str, name: &str) -> Option<u64> {
    let sql = format!("SELECT count(*) FROM {}.{}", qident(schema), qident(name));
    let result = harbor_client::query(conn, &sql).ok()?;
    result.rows.first()?.first()?.as_u64()
}

/// Row counts for every table in one query, for the sidebar. DuckDB's
/// `estimated_size` matched exact COUNT(*) on every live table probed,
/// and the sidebar rounds to SI anyway.
pub(crate) fn table_counts(
    conn: &Conn,
) -> Option<std::collections::HashMap<(String, String), u64>> {
    let result = harbor_client::query(
        conn,
        "SELECT schema_name, table_name, estimated_size FROM duckdb_tables()",
    )
    .ok()?;
    Some(
        result
            .rows
            .iter()
            .filter_map(|row| {
                let schema = row.first()?.as_str()?.to_string();
                let table = row.get(1)?.as_str()?.to_string();
                let n = row.get(2)?.as_u64()?;
                Some(((schema, table), n))
            })
            .collect(),
    )
}

/// `PRAGMA database_size` -> (data bytes, wal bytes), for the identity
/// card. The server prints binary-pretty strings ("175.0 MiB"); parsing
/// to bytes lets the app render its own decimal units everywhere.
pub(crate) fn database_size(conn: &Conn) -> Option<(u64, u64)> {
    let result = harbor_client::query(conn, "PRAGMA database_size").ok()?;
    let col = |key: &str| {
        result
            .columns
            .iter()
            .position(|c| c.name.as_deref() == Some(key))
            .and_then(|i| result.rows.first()?.get(i).cloned())
            .and_then(|v| match v {
                Value::String(s) => crate::util::parse_pretty_size(&s),
                other => other.as_u64(),
            })
    };
    Some((col("database_size")?, col("wal_size")?))
}
