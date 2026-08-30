//! Client-side SQL against a berth: text construction (quoting, paging)
//! and result shaping. One home for every query the app hand-writes, so
//! the render files (grid, sidebar, content) never own SQL for another
//! surface. All of these block; callers run them on a background thread.

use crate::util::qident;
use harbor_client::Conn;

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
