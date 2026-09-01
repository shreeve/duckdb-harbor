//! The v2 connection layer: open, clone, cache, batch, and cancellation.
//! Skips (with a note) when the resolvable engine predates the v2 C API.

use std::path::Path;
use std::time::{Duration, Instant};

use harbor::v2::conn::{self, Param};
use harbor::v2::{engine, ffi};

fn ready() -> bool {
    match engine() {
        Ok(_) => true,
        Err(e) => {
            eprintln!("v2_conn: skipped — {e}");
            false
        }
    }
}

/// Run one statement, return each row as its encoded JSON values line.
fn rows(c: &mut conn::Conn, sql: &str, params: &[Param]) -> Result<Vec<String>, harbor::v2::Error> {
    let api = &engine().unwrap().api;
    let stmts = c.statements(sql)?;
    let mut out = Vec::new();
    for stmt in stmts.iter() {
        let mut stream = c.execute(stmt, params)?;
        // Encode through the shared cell encoder so this test exercises the
        // same path the server will.
        let columns = std::mem::take(&mut stream.columns);
        let types: Vec<&harbor::v2::encode::Type> = columns.iter().map(|(_, t)| t).collect();
        while let Some(chunk) = stream.next_chunk()? {
            let readers = chunk.readers(types.len())?;
            for row in 0..chunk.rows {
                let mut line = String::new();
                for (i, ty) in types.iter().enumerate() {
                    if i > 0 {
                        line.push(',');
                    }
                    harbor::v2::encode::emit_cell(&mut line, api, &readers[i], ty, row)?;
                }
                out.push(line);
            }
        }
    }
    Ok(out)
}

#[test]
fn open_clone_query_and_params() {
    if !ready() {
        return;
    }
    let mut c = conn::open(Path::new(":memory:"), &[]).expect("open");
    assert!(c.engine_version().starts_with("v2."));

    // Params bind positionally; NULL binds as a typed null.
    let got = rows(&mut c, "SELECT $1 + 1, $2, $3", &[
        Param::I64(41),
        Param::Text("hi".into()),
        Param::Null,
    ])
    .unwrap();
    assert_eq!(got, [r#"42,"hi",null"#]);

    // A clone shares the database: DDL on one is visible on the other.
    let mut c2 = c.try_clone().expect("clone");
    rows(&mut c, "CREATE TABLE t(a INTEGER)", &[]).unwrap();
    rows(&mut c, "INSERT INTO t VALUES (7)", &[]).unwrap();
    assert_eq!(rows(&mut c2, "SELECT a FROM t", &[]).unwrap(), ["7"]);

    // The parse cache returns reusable statements; catalog changes are seen
    // because execution re-binds: same text, different answer after INSERT.
    assert_eq!(rows(&mut c2, "SELECT count(*)::INTEGER FROM t", &[]).unwrap(), ["1"]);
    rows(&mut c, "INSERT INTO t VALUES (8)", &[]).unwrap();
    assert_eq!(rows(&mut c2, "SELECT count(*)::INTEGER FROM t", &[]).unwrap(), ["2"]);

    // execute_batch drains multi-statement text.
    c.execute_batch("CREATE TABLE u(x INTEGER); INSERT INTO u VALUES (1),(2); CHECKPOINT")
        .unwrap();
    assert_eq!(rows(&mut c2, "SELECT sum(x)::INTEGER FROM u", &[]).unwrap(), ["3"]);
}

#[test]
fn set_option_reaches_the_engine() {
    if !ready() {
        return;
    }
    let mut c = conn::open(Path::new(":memory:"), &[]).expect("open");
    c.set_option("memory_limit", "123MiB").expect("set_option");
    let got = rows(&mut c, "SELECT current_setting('memory_limit')", &[]).unwrap();
    assert_eq!(got, [r#""123.0 MiB""#]);
}

#[test]
fn interrupt_crosses_threads() {
    if !ready() {
        return;
    }
    let mut c = conn::open(Path::new(":memory:"), &[]).expect("open");
    let handle = c.interrupt_handle();

    // Fire the interrupt shortly after the query starts, from another thread.
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        handle.interrupt();
    });

    let started = Instant::now();
    let err = rows(&mut c, "SELECT count(*) FROM range(10000000000)", &[])
        .expect_err("a ten-billion-row count should not finish in 300ms");
    canceller.join().unwrap();

    assert!(started.elapsed() < Duration::from_secs(30), "interrupt did not land");
    assert_eq!(err.code, ffi::ERROR_RUNTIME_INTERRUPT, "unexpected error: {err}");

    // The connection survives its cancelled query.
    assert_eq!(rows(&mut c, "SELECT 1", &[]).unwrap(), ["1"]);
}

#[test]
fn a_dropped_conn_defuses_its_interrupt_handle() {
    if !ready() {
        return;
    }
    let c = conn::open(Path::new(":memory:"), &[]).expect("open");
    let handle = c.interrupt_handle();
    drop(c);
    // Aims at nothing; must not crash.
    handle.interrupt();
}
