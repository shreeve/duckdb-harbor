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
    sorted_page_sql(source, rowid, filter, &[], &[], page, size)
}

/// [`page_sql`] under a header sort. The page has one ordering: the header
/// sort's ([`order_by`]) when it has keys, and the strip's own ORDER BY,
/// as typed, when it has none. The two are not joined. The strip's is raw
/// SQL that may take no key after it (`ORDER BY ALL`), and a header sort
/// that only broke the strip's ties would not be the order its arrows
/// show. The grid marks a strip ORDER BY the header sort sets aside.
pub(crate) fn sorted_page_sql(
    source: &str,
    rowid: bool,
    filter: &Option<String>,
    sort: &[SortKey],
    tiebreak: &[String],
    page: usize,
    size: usize,
) -> String {
    let cols = if rowid { "[rowid::UBIGINT, hash(*COLUMNS(*))] AS rowid, *" } else { "*" };
    let (cond, strip) = filter.as_deref().map_or(("", None), split_order);
    let order = order_by(sort, tiebreak).or_else(|| strip.map(|o| o.trim().to_string()));
    let order = order.map_or(String::new(), |o| format!("\nORDER BY {o}\n"));
    format!(
        "SELECT {cols} FROM {source}{}{order} LIMIT {size} OFFSET {}",
        where_part(cond),
        page * size
    )
}

/// One key of a header sort: a column of the table, by name, and its
/// direction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SortKey {
    pub(crate) column: String,
    pub(crate) desc: bool,
}

/// The ORDER BY a header sort reads as, or None when it has no keys. Each
/// key is its column quoted, its direction and `NULLS LAST`, which is
/// DuckDB's default in both directions, spelled out so that a
/// `default_null_order` set on the server cannot turn an arrow around. The
/// tie-break follows, ascending: each column of `tiebreak` ([`tiebreak`])
/// the keys do not already name. Rows the keys tie then keep one order
/// from one page's query to the next, so a page boundary falls in one
/// place and every row is on exactly one page.
pub(crate) fn order_by(sort: &[SortKey], tiebreak: &[String]) -> Option<String> {
    if sort.is_empty() {
        return None;
    }
    let keys = sort.iter().map(|k| {
        format!("{} {} NULLS LAST", qident(&k.column), if k.desc { "DESC" } else { "ASC" })
    });
    let ties = tiebreak
        .iter()
        .filter(|c| !sort.iter().any(|k| &k.column == *c))
        .map(|c| format!("{} ASC", qident(c)));
    Some(keys.chain(ties).collect::<Vec<_>>().join(", "))
}

/// Whether a page read without the sort lacks a column the sort names:
/// what tells a sort the table cannot take from a failure of
/// another kind, which a page read again unsorted would hide.
pub(crate) fn sort_column_gone(sort: &[SortKey], columns: &[wire::Column]) -> bool {
    sort.iter().any(|k| !columns.iter().any(|c| c.name.as_deref() == Some(k.column.as_str())))
}

/// What breaks a header sort's ties: the table's identity when it has one,
/// its primary key or the `rowid` a keyless table is edited by, and every
/// column when it has neither (a keyless table with a column of its own
/// named `rowid`). Rows equal in every column cannot be told apart, so the
/// order of the values a page shows is fixed even where the order of the
/// rows is not.
pub(crate) fn tiebreak(identity: &[String], columns: &[String]) -> Vec<String> {
    if identity.is_empty() { columns.to_vec() } else { identity.to_vec() }
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
/// and the ordering after it, which sorts the page while the header has no
/// sort of its own: `x > 0 ORDER BY name`, or `ORDER BY name` alone. The
/// cut is read in code only (`wire::scan`), so an ORDER BY
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

/// Fetch a table's first page under the sort remembered for it; app.rs
/// calls this before it builds the grid (DESIGN.md: fetch first, commit
/// over the old value). A sort the table cannot take, one naming a
/// column another client dropped since the catalog was read, does not keep
/// the table from opening: the page is read again unsorted, and taken when
/// it lacks a column the sort names ([`sort_column_gone`]). The sort
/// returned is the one the page was read under.
pub(crate) fn first_page(
    conn: &Conn,
    schema: &str,
    name: &str,
    rowid: bool,
    sort: Vec<SortKey>,
    tiebreak: &[String],
    limit: usize,
) -> (Result<harbor_client::QueryResult, String>, Vec<SortKey>) {
    let source = source(schema, name);
    let read = |sort: &[SortKey]| {
        harbor_client::query(conn, &sorted_page_sql(&source, rowid, &None, sort, tiebreak, 0, limit))
    };
    match read(&sort) {
        Err(message) if !sort.is_empty() => match read(&[]) {
            Ok(page) if sort_column_gone(&sort, &page.columns) => (Ok(page), Vec::new()),
            _ => (Err(message), sort),
        },
        page => (page, sort),
    }
}

/// The table's exact row count, for the status line.
pub(crate) fn total_rows(conn: &Conn, schema: &str, name: &str) -> Option<u64> {
    count_of(&harbor_client::query(conn, &count_sql(&source(schema, name), &None)).ok()?)
}

#[cfg(test)]
mod tests {
    use super::{
        count_sql, order_by, page_sql, query_source, sorted_page_sql, split_order, tiebreak, SortKey,
    };

    fn key(column: &str, desc: bool) -> SortKey {
        SortKey { column: column.to_string(), desc }
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn a_header_sort_quotes_its_keys_states_nulls_and_ends_in_the_tiebreak() {
        assert_eq!(order_by(&[], &names(&["id"])), None);
        assert_eq!(
            order_by(&[key("Name", true), key("a\"b", false)], &names(&["id"])),
            Some("\"Name\" DESC NULLS LAST, \"a\"\"b\" ASC NULLS LAST, \"id\" ASC".into())
        );
        // A tie-break column the keys already name is not named again.
        assert_eq!(
            order_by(&[key("b", true)], &names(&["a", "b"])),
            Some("\"b\" DESC NULLS LAST, \"a\" ASC".into())
        );
        // The identity breaks ties; with none, every column does.
        assert_eq!(tiebreak(&names(&["id"]), &names(&["id", "v"])), names(&["id"]));
        assert_eq!(tiebreak(&names(&["rowid"]), &names(&["rowid", "v"])), names(&["rowid"]));
        assert_eq!(tiebreak(&[], &names(&["rowid", "v"])), names(&["rowid", "v"]));
    }

    #[test]
    fn a_sort_is_given_up_only_for_a_column_the_page_lacks() {
        let cols: Vec<wire::Column> = ["id", "v"]
            .map(|n| wire::Column { name: Some(n.to_string()), ..Default::default() })
            .to_vec();
        assert!(!super::sort_column_gone(&[key("v", true), key("id", false)], &cols));
        assert!(super::sort_column_gone(&[key("v", true), key("gone", false)], &cols));
    }

    #[test]
    fn the_header_sort_sets_the_strips_order_aside_and_never_reaches_the_count() {
        let sort = [key("v", false)];
        let ties = names(&["id"]);
        // No strip: the header's ordering alone, on lines of its own.
        assert_eq!(
            sorted_page_sql("t", false, &None, &sort, &ties, 2, 5),
            "SELECT * FROM t\nORDER BY \"v\" ASC NULLS LAST, \"id\" ASC\n LIMIT 5 OFFSET 10"
        );
        // A strip with a condition and an ORDER BY: the condition filters,
        // the header orders, and the strip's ORDER BY is not joined to it.
        let filter = Some("x > 0 ORDER BY ALL -- all".to_string());
        assert_eq!(
            sorted_page_sql("t", true, &filter, &sort, &ties, 0, 5),
            "SELECT [rowid::UBIGINT, hash(*COLUMNS(*))] AS rowid, * FROM t WHERE (\nx > 0\n)\nORDER BY \"v\" ASC NULLS LAST, \"id\" ASC\n LIMIT 5 OFFSET 0"
        );
        // With no header keys the strip's ORDER BY sorts, as typed, with no
        // tie-break appended to the user's own SQL.
        assert_eq!(
            sorted_page_sql("t", false, &filter, &[], &ties, 0, 5),
            page_sql("t", false, &filter, 0, 5)
        );
        assert!(page_sql("t", false, &filter, 0, 5).ends_with("ORDER BY ALL -- all\n LIMIT 5 OFFSET 0"));
        // The count counts what the condition keeps, whatever orders it.
        assert_eq!(count_sql("t", &filter), "SELECT count(*) FROM t WHERE (\nx > 0\n)");
    }

    /// A sorted table paged with ties on its sort key, NULLs among them,
    /// shows every row exactly once, in the order the arrows say, for each
    /// kind of tie-break: a primary key, the rowid identity of a keyless
    /// table, and every column of a keyless table whose own `rowid` column
    /// shadows DuckDB's. Live: needs `HARBOR_LIVE_DB`, a scratch database
    /// file, as the probes in harbor-client's `tests/live.rs` do.
    #[test]
    #[ignore]
    fn a_sorted_table_with_ties_pages_every_row_exactly_once() {
        let Some(db) = std::env::var_os("HARBOR_LIVE_DB") else {
            println!("set HARBOR_LIVE_DB to a scratch database file; skipping");
            return;
        };
        let conn = harbor_client::fleet::connect_path(std::path::Path::new(&db)).expect("connect");
        let alone = |sql: &str| harbor_client::query(&conn, sql).expect(sql);
        // 300,000 rows over 7 values of g and NULL, inserted out of order:
        // enough for the engine to sort in parallel, where rows that tie
        // come back in another order from one run of a query to the next.
        let rows = "SELECT (i * 7919) % 300000 AS id, CASE WHEN i % 8 = 0 THEN NULL ELSE i % 7 END AS g, \
                    i % 3 AS h FROM range(300000) r(i)";
        alone("CREATE OR REPLACE TABLE _dt_sort_key(id INTEGER PRIMARY KEY, g INTEGER, h INTEGER)");
        alone(&format!("INSERT INTO _dt_sort_key {rows}"));
        alone(&format!("CREATE OR REPLACE TABLE _dt_sort_rowid AS SELECT g, h FROM ({rows})"));
        alone(&format!("CREATE OR REPLACE TABLE _dt_sort_shadow AS SELECT id AS rowid, g, h FROM ({rows})"));
        // Rows equal in every column, for the shadowed table to tell apart
        // by value only.
        alone("INSERT INTO _dt_sort_shadow SELECT * FROM _dt_sort_shadow WHERE rowid < 100");

        let sort = [key("g", true), key("h", false)];
        for (table, rowid, identity, size) in [
            ("_dt_sort_key", false, names(&["id"]), 50_000),
            ("_dt_sort_rowid", true, names(&["rowid"]), 37_000),
            ("_dt_sort_shadow", false, Vec::new(), 64_000),
        ] {
            let source = super::source("main", table);
            let columns: Vec<String> = alone(&format!("SELECT * FROM {source} LIMIT 0"))
                .columns
                .iter()
                .filter_map(|c| c.name.clone())
                .collect();
            let ties = tiebreak(&identity, &columns);
            let total = super::count_of(&alone(&count_sql(&source, &None))).expect("count") as usize;
            let mut paged = Vec::new();
            for page in 0..total.div_ceil(size) + 1 {
                let sql = sorted_page_sql(&source, rowid, &None, &sort, &ties, page, size);
                paged.extend(alone(&sql).rows);
            }
            assert_eq!(paged.len(), total, "{table}: every row once, and no more");
            // The order the arrows say: g descending with NULLs last, then h.
            // A keyless table's page leads with the hidden identity column.
            let at = |r: &Vec<serde_json::Value>, name: &str| {
                let ix = columns.iter().position(|c| c == name).unwrap() + rowid as usize;
                r[ix].as_i64()
            };
            for pair in paged.windows(2) {
                let (a, b) = (&pair[0], &pair[1]);
                let (ga, gb) = (at(a, "g"), at(b, "g"));
                let ordered = match (ga, gb) {
                    (Some(x), Some(y)) => x > y || (x == y && at(a, "h") <= at(b, "h")),
                    (Some(_), None) => true,
                    (None, Some(_)) => false,
                    (None, None) => at(a, "h") <= at(b, "h"),
                };
                assert!(ordered, "{table}: {a:?} before {b:?}");
            }
            // Every row exactly once: the pages hold the table's multiset.
            let whole = alone(&format!(
                "SELECT {} FROM {source}",
                if rowid { "[rowid::UBIGINT, hash(*COLUMNS(*))] AS rowid, *" } else { "*" }
            ))
            .rows;
            let text = |rows: &[Vec<serde_json::Value>]| {
                let mut t: Vec<String> = rows.iter().map(|r| serde_json::to_string(r).unwrap()).collect();
                t.sort();
                t
            };
            assert_eq!(text(&paged), text(&whole), "{table}: the pages are the table");
        }
        // A remembered sort opens the table under it; one naming a column
        // the table does not have opens it unsorted, and is dropped.
        let ties = names(&["id"]);
        let (page, kept) = super::first_page(&conn, "main", "_dt_sort_key", false, sort.to_vec(), &ties, 3);
        assert_eq!((page.expect("sorted").rows.len(), kept), (3, sort.to_vec()));
        let gone = vec![key("gone", false)];
        let (page, kept) = super::first_page(&conn, "main", "_dt_sort_key", false, gone, &ties, 3);
        assert_eq!((page.expect("unsorted").rows.len(), kept), (3, Vec::new()));
        for table in ["_dt_sort_key", "_dt_sort_rowid", "_dt_sort_shadow"] {
            alone(&format!("DROP TABLE {table}"));
        }
    }

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
