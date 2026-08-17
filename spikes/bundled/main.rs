// Phase 0 spike #1: embedded DuckDB through the same library API the
// extension's server core uses (Connection, try_clone, typed rows, and the
// complex-type surface emit_value walks: LIST, STRUCT, HUGEINT, DECIMAL).
use duckdb::Connection;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::open_in_memory()?;
    let version: String = conn.query_row("SELECT version()", [], |r| r.get(0))?;

    conn.execute_batch(
        "CREATE TABLE t(id BIGINT, name TEXT);
         INSERT INTO t VALUES (1,'a'),(2,'b'),(3,'c');",
    )?;

    // try_clone is how the pool mints worker connections from one instance
    let worker = conn.try_clone()?;
    let n: i64 = worker.query_row("SELECT count(*) FROM t", [], |r| r.get(0))?;
    assert_eq!(n, 3);

    // params + the complex types the NDJSON emitter has to walk
    let mut stmt = worker.prepare(
        "SELECT id, name,
                [1,2,3]                        AS l,
                {'a': 1, 'b': 'x'}             AS s,
                (170141183460469231731687303715884105727::HUGEINT)::VARCHAR AS h,
                (1.5::DECIMAL(12,3))::VARCHAR  AS d
         FROM t WHERE id > ? ORDER BY id",
    )?;
    let mut rows = stmt.query([1i64])?;
    let mut seen = 0;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let name: String = row.get(1)?;
        let h: String = row.get(4)?;
        assert!(id > 1 && !name.is_empty() && h.ends_with("727"));
        seen += 1;
    }
    assert_eq!(seen, 2);

    println!("BUNDLED SPIKE PASS: embedded {version}, pool-style try_clone + typed rows OK");
    Ok(())
}
