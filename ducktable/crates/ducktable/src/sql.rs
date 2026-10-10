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

/// A query's FROM target: the user's statement, parenthesized. The Query
/// view's results grid is "a Data window with a custom query preceding
/// it" — the same page_sql machinery pages any SELECT-shaped statement
/// by treating it as a subquery: `SELECT * FROM (statement) LIMIT …`.
///
/// The closing paren sits on its OWN LINE: a statement ending in a line
/// comment would otherwise swallow the paren and everything page_sql
/// appends after it. (No `;` stripping — the statement splitter never
/// includes a top-level terminator, so any trailing semicolon here is
/// inside a comment and must be left alone.)
pub(crate) fn query_source(sql: &str) -> String {
    format!("(\n{}\n)", sql.trim())
}

/// The SELECT for one page of `source` under an optional filter. The
/// filter text splices in verbatim BY DESIGN: the strip is a raw SQL
/// surface and the berth is the user's own database — the author of the
/// WHERE clause is the person it could affect. Its condition sits in
/// parentheses on lines of its own, and an ORDER BY that ends it
/// ([`split_order`]) on lines of its own after them, so a filter ending in
/// a line comment cannot reach the LIMIT after it, and the page stays
/// bounded.
///
/// `rowid` prepends the editing identity for a table without a primary
/// key (docs/EDITING.md): DuckDB's implicit rowid paired with a hash of
/// the whole row, in one column named `rowid`. A rowid alone only names a
/// position, and a checkpoint that compacts deleted rows renumbers the
/// rest; the hash is what tells the row at that position is still the one
/// fetched. The grid hides the column; only the WHERE clauses see it.
pub(crate) fn page_sql(
    source: &str,
    rowid: bool,
    filter: &Option<String>,
    page: usize,
    size: usize,
) -> String {
    let cols = if rowid { "[rowid::UBIGINT, hash(*COLUMNS(*))] AS rowid, *" } else { "*" };
    let (cond, order) = filter.as_deref().map_or(("", None), split_order);
    let order = order.map_or(String::new(), |o| format!("\nORDER BY {}\n", o.trim()));
    format!(
        "SELECT {cols} FROM {source}{}{order} LIMIT {size} OFFSET {}",
        where_part(cond),
        page * size
    )
}

/// The count under a filter counts what its condition keeps; its ORDER BY
/// orders nothing a count reads.
pub(crate) fn count_sql(source: &str, filter: &Option<String>) -> String {
    let cond = filter.as_deref().map_or("", |f| split_order(f).0);
    format!("SELECT count(*) FROM {source}{}", where_part(cond))
}

fn where_part(cond: &str) -> String {
    let mut end = 0;
    wire::statement::skip_trivia(cond.as_bytes(), &mut end);
    if end == cond.len() { String::new() } else { format!(" WHERE (\n{cond}\n)") }
}

/// A filter cut at an ORDER BY at its top level: the condition before it,
/// and the ordering after it. The grid has no sort of its own, so the strip
/// is where a Data view is sorted: `x > 0 ORDER BY name`, or `ORDER BY
/// name` alone. The cut is read in code only (`wire::scan`), so an ORDER BY
/// in a string, a comment or a parenthesis (a window, an aggregate, a
/// subquery) stays in the condition. A filter whose parentheses close more
/// than they open is not cut: whole inside the parentheses, it is the
/// engine's syntax error, and never a way past the page's LIMIT.
pub(crate) fn split_order(filter: &str) -> (&str, Option<&str>) {
    use wire::statement::{bare_word, skip_trivia};
    let b = filter.as_bytes();
    let (mut depth, mut cut) = (0i32, None);
    for span in wire::scan::scan(filter).into_iter().filter(|s| s.kind == wire::scan::Kind::Code) {
        let mut i = span.start;
        while i < span.end {
            // Spaces as the engine reads them, Unicode ones included.
            let at = i;
            skip_trivia(b, &mut i);
            if i > at || i >= span.end {
                continue;
            }
            match b[i] {
                b'(' => (depth, i) = (depth + 1, i + 1),
                b')' if depth == 0 => return (filter, None),
                b')' => (depth, i) = (depth - 1, i + 1),
                c if c.is_ascii_punctuation() && !matches!(c, b'_' | b'$') => i += 1,
                _ => {
                    let at = i;
                    if bare_word(b, &mut i) == "ORDER" && depth == 0 {
                        let mut by = i;
                        if bare_word(b, &mut by) == "BY" {
                            cut = Some((at, by));
                        }
                    }
                }
            }
        }
    }
    match cut {
        Some((at, by)) => (filter[..at].trim_end(), Some(&filter[by..])),
        None => (filter, None),
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

#[cfg(test)]
mod tests {
    use super::{count_sql, page_sql, query_source, split_order};

    #[test]
    fn a_filter_ending_in_a_comment_keeps_the_page_bounded() {
        let filter = Some("id > 0 -- only positive".to_string());
        let sql = page_sql("\"main\".\"t\"", false, &filter, 2, 500);
        assert_eq!(sql, "SELECT * FROM \"main\".\"t\" WHERE (\nid > 0 -- only positive\n) LIMIT 500 OFFSET 1000");
        // The comment ends at its line, before the LIMIT.
        assert!(sql.lines().last().unwrap().starts_with(") LIMIT 500"), "{sql}");
        // An OR stays inside the filter, in the count as in the page.
        let or = Some("a = 1 OR b = 2".to_string());
        assert_eq!(count_sql("t", &or), "SELECT count(*) FROM t WHERE (\na = 1 OR b = 2\n)");
    }

    #[test]
    fn trailing_line_comment_cannot_eat_the_paging_clause() {
        // A statement ending in a comment must not swallow the closing paren
        // and the LIMIT/OFFSET page_sql appends.
        let src = query_source("from members\n-- where first_name ilike '%s%';");
        let sql = page_sql(&src, false, &None, 1, 5000);
        assert!(sql.ends_with(") LIMIT 5000 OFFSET 5000"), "{sql}");
        // And the comment's own semicolon stays: it is comment text.
        assert!(sql.contains("'%s%';"), "{sql}");
    }

    #[test]
    fn an_order_by_ending_the_filter_sorts_outside_its_parentheses() {
        let filter = Some("x > 0 ORDER BY name DESC".to_string());
        assert_eq!(
            page_sql("t", false, &filter, 1, 5),
            "SELECT * FROM t WHERE (\nx > 0\n)\nORDER BY name DESC\n LIMIT 5 OFFSET 5"
        );
        // The count counts what the condition keeps, unordered.
        assert_eq!(count_sql("t", &filter), "SELECT count(*) FROM t WHERE (\nx > 0\n)");
        // A line comment after the ordering ends at its line, before the LIMIT.
        let commented = Some("x > 0 ORDER BY name -- by name".to_string());
        let sql = page_sql("t", false, &commented, 0, 5);
        assert!(sql.ends_with("ORDER BY name -- by name\n LIMIT 5 OFFSET 0"), "{sql}");
        // An ordering alone sorts the whole table, with no WHERE.
        let alone = Some("order /* c */ by id".to_string());
        assert_eq!(page_sql("t", false, &alone, 0, 5), "SELECT * FROM t\nORDER BY id\n LIMIT 5 OFFSET 0");
        assert_eq!(count_sql("t", &alone), "SELECT count(*) FROM t");
    }

    #[test]
    fn only_an_order_by_at_the_top_level_of_code_is_cut() {
        for whole in [
            "row_number() OVER (ORDER BY id) < 3",
            "name = 'x ORDER BY y'",
            "\"ORDER BY\" = 1",
            "x > 0 -- ORDER BY y",
            "x > 0 /* ORDER BY y */",
            "border BY 1",
            "id IN (SELECT id FROM u ORDER BY id LIMIT 3)",
            // Parentheses that close more than they open stay whole, the
            // engine's syntax error inside the page's parentheses.
            "x) ORDER BY (y",
        ] {
            assert_eq!(split_order(whole), (whole, None), "{whole}");
        }
        assert_eq!(split_order("a ORDER BY b ORDER BY c"), ("a ORDER BY b", Some(" c")));
        assert_eq!(split_order("x -- c\nORDER\u{a0}BY y"), ("x -- c", Some(" y")));
    }
}
