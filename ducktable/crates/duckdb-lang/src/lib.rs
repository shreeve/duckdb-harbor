//! tree-sitter-duckdb: the first tree-sitter grammar for DuckDB SQL,
//! derived from DuckDB's own PEG grammar (vendored under `upstream/`,
//! pinned by `upstream/COMMIT`). Node names mirror the PEG rule names
//! snake_cased; docs/QUERY.md in the workspace root tells the story.
//!
//! The parser is generated C (`grammar/src/parser.c`), checked in and
//! compiled by build.rs — consumers need no tree-sitter CLI.

use tree_sitter_language::LanguageFn;

unsafe extern "C" {
    fn tree_sitter_duckdb() -> *const ();
}

/// The DuckDB grammar, ready for `tree_sitter::Language::new(LANGUAGE)`
/// or gpui-component's `LanguageConfig`.
pub const LANGUAGE: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_duckdb) };

/// Highlight query targeting gpui-component's recognized capture names.
pub const HIGHLIGHTS: &str = include_str!("../queries/highlights.scm");

/// Injections and locals: none yet (phase 2).
pub const INJECTIONS: &str = "";
pub const LOCALS: &str = "";

#[cfg(test)]
mod tests {
    use super::*;

    fn language() -> tree_sitter::Language {
        tree_sitter::Language::new(LANGUAGE)
    }

    fn parse(sql: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language()).unwrap();
        parser.parse(sql, None).unwrap()
    }

    /// The week-1 acceptance bar: the signature DuckDB-isms parse with
    /// zero ERROR nodes. Each failure here is a grammar bug, not a
    /// corpus bug — these are all engine-valid statements.
    #[test]
    fn duckdb_isms_parse_clean() {
        let corpus = [
            "FROM trips SELECT city, count(*) GROUP BY ALL ORDER BY ALL",
            "FROM trips",
            "SELECT * EXCLUDE (secret), price * 1.1 AS bumped FROM products",
            "SELECT list_transform([1,2,3], x -> x + 1)",
            "SELECT {'a': 1, 'b': [1,2,3]}, MAP {1: 'one'}",
            "SELECT t.* REPLACE (upper(name) AS name) FROM t QUALIFY row_number() OVER (PARTITION BY city) = 1",
            "WITH cte AS (SELECT 1 AS x) SELECT cte.x FROM cte WHERE x IS NOT NULL AND x NOT IN (2, 3)",
            "SELECT read_csv('f.csv', header := true) FROM read_parquet('x.parquet')",
            "select lower(name) from \"my table\" as t(a, b,) limit 10 offset 5",
            "INSERT INTO t BY NAME (SELECT 1 AS a) ON CONFLICT DO NOTHING RETURNING *",
            "UPDATE t SET a = 5, b = a + 1 FROM u WHERE t.id = u.id RETURNING a",
            "DELETE FROM t USING u WHERE t.id = u.id",
            "CREATE OR REPLACE TABLE t (id INTEGER PRIMARY KEY, name VARCHAR NOT NULL DEFAULT 'x', tags VARCHAR[], meta STRUCT(a INT, b VARCHAR))",
            "CREATE TABLE t2 AS SELECT * FROM t",
            "CREATE MACRO add1(x) AS x + 1",
            "ATTACH 'db.duckdb' AS mydb (READ_ONLY)",
            "USE mydb.main",
            "SET memory_limit = '4GB'",
            "PRAGMA table_info('t')",
            "CALL enable_peg_parser()",
            "EXPLAIN ANALYZE SELECT 1",
            "COPY t TO 'out.parquet' (FORMAT parquet)",
            "SUMMARIZE SELECT * FROM t",
            "SELECT CAST(x AS DECIMAL(10,2)), x::VARCHAR, arr[1], arr[1:2], s.field FROM t",
            "SELECT count(DISTINCT a ORDER BY b) FILTER (WHERE c > 0) OVER w FROM t WINDOW w AS (PARTITION BY d)",
            "SELECT * FROM a JOIN b ON a.id = b.id LEFT JOIN c USING (id) CROSS JOIN d",
            "SELECT * FROM t USING SAMPLE 10%",
            "SELECT [x FOR x IN [1,2,3] IF x > 1]",
            "SELECT 1 UNION ALL SELECT 2 UNION BY NAME SELECT 3 INTERSECT SELECT 4",
            "1 + 1",
            "TABLE trips",
            "VALUES (1, 'a'), (2, 'b')",
            "SELECT $1, ?, $name, #1",
            "SELECT e'\\n', 'it''s', $$raw string$$",
            "SELECT 0x1F, 1_000_000, 1.5e10, .5",
        ];
        for sql in corpus {
            let tree = parse(sql);
            assert!(
                !tree.root_node().has_error(),
                "ERROR nodes in: {sql}\n{}",
                tree.root_node().to_sexp()
            );
        }
    }

    /// Broken text still yields a tree (the whole point of the lens).
    #[test]
    fn broken_text_recovers() {
        // Note "SELECT foo FR" is NOT broken — FR is an alias. This is.
        let tree = parse("SELECT foo FROM WHERE (((");
        assert!(tree.root_node().has_error());
        // The statement start is still recognized.
        assert!(tree.root_node().to_sexp().contains("select"));
    }

    /// The highlight query compiles against the grammar — a rename in
    /// grammar.js that orphans a capture fails here, not at app runtime.
    #[test]
    fn highlight_query_compiles() {
        tree_sitter::Query::new(&language(), HIGHLIGHTS).expect("highlights.scm must compile");
    }
}
