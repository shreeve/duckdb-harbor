//! Client-side SQL against a berth: text construction (quoting, paging)
//! and result shaping. One home for every query the app hand-writes, so
//! the render files (grid, sidebar, content) never own SQL for another
//! surface. All of these block; callers run them on a background thread.

use crate::util::qident;
use harbor_client::Conn;

/// A table's FROM target, schema-qualified and quoted.
pub(crate) fn source(schema: &str, name: &str) -> String {
    format!("{}.{}", qident(schema), qident(name))
}

/// The SELECT for one page of `source` under an optional filter. The
/// filter text splices in verbatim BY DESIGN: the strip is a raw SQL
/// surface and the berth is the user's own database — the author of the
/// WHERE clause is the person it could affect.
///
/// `rowid` prepends DuckDB's implicit row identifier — the editing
/// identity for tables without a primary key (docs/EDITING.md). The
/// grid hides that column; only the WHERE clauses ever see it.
pub(crate) fn page_sql(
    source: &str,
    rowid: bool,
    filter: &Option<String>,
    page: usize,
    size: usize,
) -> String {
    let cols = if rowid { "rowid, *" } else { "*" };
    format!(
        "SELECT {cols} FROM {source}{} LIMIT {size} OFFSET {}",
        where_part(filter),
        page * size
    )
}

pub(crate) fn count_sql(source: &str, filter: &Option<String>) -> String {
    format!("SELECT count(*) FROM {source}{}", where_part(filter))
}

fn where_part(filter: &Option<String>) -> String {
    match filter {
        Some(f) => format!(" WHERE {f}"),
        None => String::new(),
    }
}

/// The one cell a count(*) answers with.
pub(crate) fn count_of(result: &harbor_client::QueryResult) -> Option<u64> {
    result.rows.first()?.first()?.as_u64()
}

/// Fetch a table's first page; app.rs calls this before it builds the
/// grid (DESIGN.md: fetch first, commit over the old value).
pub(crate) fn first_page(
    conn: &Conn,
    schema: &str,
    name: &str,
    rowid: bool,
    limit: usize,
) -> Result<harbor_client::QueryResult, String> {
    harbor_client::query(conn, &page_sql(&source(schema, name), rowid, &None, 0, limit))
}

/// The table's exact row count, for the status line.
pub(crate) fn total_rows(conn: &Conn, schema: &str, name: &str) -> Option<u64> {
    count_of(&harbor_client::query(conn, &count_sql(&source(schema, name), &None)).ok()?)
}
