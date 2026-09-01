//! v2 engine smoke: the full result path through the generated FFI.
//!
//! env -> open(:memory:) -> connect -> parse -> execute -> fetch -> view,
//! then teardown in reverse — every fallible call checked. Skips (passes
//! vacuously, with a note) when the resolvable engine predates the v2 C API,
//! so the suite stays green on v1-era libs; point HARBOR_LIBDUCKDB at a
//! v2-bearing build to make it bite.

use harbor::v2::{Engine, Error, bytes_view, engine, ffi};

/// Check a v2 call: on failure, fold the error_info into a panic message.
macro_rules! ok {
    ($eng:expr, $call:ident($($arg:expr),*)) => {{
        let mut err: ffi::error_info_handle = std::ptr::null_mut();
        let code = unsafe { ($eng.api.$call.expect(stringify!($call)))($($arg,)* &mut err) };
        assert!(
            code == ffi::ERROR_NONE,
            "{}: {}", stringify!($call), Error::take(&$eng.api, code, err)
        );
    }};
}

fn v2_engine() -> Option<&'static Engine> {
    match engine() {
        Ok(e) => Some(e),
        Err(e) => {
            eprintln!("v2_smoke: skipped — {e}");
            None
        }
    }
}

#[test]
fn the_answer_is_42_end_to_end() {
    let Some(eng) = v2_engine() else { return };
    eprintln!("v2_smoke: {} ({} v2 symbols) at {}", eng.version, eng.symbols, eng.path.display());

    let mut env: ffi::environment_handle = std::ptr::null_mut();
    ok!(eng, create_environment(&mut env));

    let memory = ffi::str_t { ptr: std::ptr::null(), len: 0 };
    let mut db: ffi::database_handle = std::ptr::null_mut();
    ok!(eng, open(env, memory, std::ptr::null_mut(), 0, &mut db));

    let mut conn: ffi::connection_handle = std::ptr::null_mut();
    ok!(eng, connect(db, &mut conn));

    let sql = c"SELECT 21 * 2 AS answer, 'harbor' AS who";
    let mut iter: ffi::statement_iterator_handle = std::ptr::null_mut();
    ok!(eng, parse_sql(conn, sql.as_ptr(), &mut iter));

    let mut stmt: ffi::sql_statement_handle = std::ptr::null_mut();
    ok!(eng, statement_iterator_next(iter, &mut stmt));
    assert!(!stmt.is_null(), "iterator yielded no statement");

    let mut result: ffi::result_handle = std::ptr::null_mut();
    ok!(eng, statement_execute(conn, stmt, std::ptr::null(), std::ptr::null(), 0, &mut result));

    let mut chunk: ffi::data_chunk_handle = std::ptr::null_mut();
    ok!(eng, result_fetch_chunk(result, &mut chunk));
    assert!(!chunk.is_null(), "no chunk before end-of-stream");

    let mut rows: ffi::idx_t = 0;
    ok!(eng, data_chunk_get_size(chunk, &mut rows));
    assert_eq!(rows, 1);

    // Column 0: INTEGER 42, read through the unified vector view.
    let mut col: ffi::vector_handle = std::ptr::null_mut();
    ok!(eng, data_chunk_get_vector(chunk, 0, &mut col));
    let mut view = ffi::vector_view_t {
        data: std::ptr::null(),
        validity: std::ptr::null(),
        sel: std::ptr::null(),
        count: 0,
    };
    ok!(eng, vector_get_view(col, &mut view));
    let row = if view.sel.is_null() { 0 } else { (unsafe { *view.sel }) as usize };
    let answer = unsafe { *(view.data as *const i32).add(row) };
    assert_eq!(answer, 42);

    // Column 1: VARCHAR, a 6-byte payload inlined in the 16-byte cell.
    ok!(eng, data_chunk_get_vector(chunk, 1, &mut col));
    ok!(eng, vector_get_view(col, &mut view));
    let row = if view.sel.is_null() { 0 } else { (unsafe { *view.sel }) as usize };
    let cell = unsafe { &*(view.data as *const ffi::bytes_t).add(row) };
    assert_eq!(unsafe { bytes_view(cell) }, b"harbor");

    // End of stream is a null chunk, not an error.
    let mut done: ffi::data_chunk_handle = std::ptr::null_mut();
    ok!(eng, result_fetch_chunk(result, &mut done));
    assert!(done.is_null(), "expected end-of-stream");

    ok_destroy(eng, chunk, result, stmt, iter, conn, db, env);
}

#[test]
fn errors_carry_code_and_text() {
    let Some(eng) = v2_engine() else { return };

    let mut env: ffi::environment_handle = std::ptr::null_mut();
    ok!(eng, create_environment(&mut env));
    let memory = ffi::str_t { ptr: std::ptr::null(), len: 0 };
    let mut db: ffi::database_handle = std::ptr::null_mut();
    ok!(eng, open(env, memory, std::ptr::null_mut(), 0, &mut db));
    let mut conn: ffi::connection_handle = std::ptr::null_mut();
    ok!(eng, connect(db, &mut conn));

    let mut iter: ffi::statement_iterator_handle = std::ptr::null_mut();
    let mut err: ffi::error_info_handle = std::ptr::null_mut();
    let code = unsafe {
        (eng.api.parse_sql.unwrap())(conn, c"SELEKT oops".as_ptr(), &mut iter, &mut err)
    };
    // Eager or incremental parsing may defer the report to iterator_next.
    let code = if code == ffi::ERROR_NONE {
        let mut stmt: ffi::sql_statement_handle = std::ptr::null_mut();
        let c = unsafe { (eng.api.statement_iterator_next.unwrap())(iter, &mut stmt, &mut err) };
        unsafe { (eng.api.sql_statement_destroy.unwrap())(&mut stmt) };
        c
    } else {
        code
    };
    assert_ne!(code, ffi::ERROR_NONE, "SELEKT parsed?!");
    let e = Error::take(&eng.api, code, err);
    assert!(!e.message.is_empty(), "error carried no text");
    eprintln!("v2_smoke: parse error as expected — {e}");

    let mut iter = iter;
    unsafe {
        (eng.api.statement_iterator_destroy.unwrap())(&mut iter);
        (eng.api.disconnect.unwrap())(&mut conn);
        (eng.api.close.unwrap())(&mut db);
        (eng.api.destroy_environment.unwrap())(&mut env);
    }
}

/// Teardown in reverse creation order; every destroy checked, since the
/// ownership contract (environment refuses while a database lives, etc.)
/// is part of what this suite proves.
#[allow(clippy::too_many_arguments)]
fn ok_destroy(
    eng: &Engine,
    mut chunk: ffi::data_chunk_handle,
    mut result: ffi::result_handle,
    mut stmt: ffi::sql_statement_handle,
    mut iter: ffi::statement_iterator_handle,
    mut conn: ffi::connection_handle,
    mut db: ffi::database_handle,
    mut env: ffi::environment_handle,
) {
    unsafe {
        assert_eq!((eng.api.data_chunk_destroy.unwrap())(&mut chunk), ffi::ERROR_NONE);
        assert_eq!((eng.api.result_destroy.unwrap())(&mut result), ffi::ERROR_NONE);
        assert_eq!((eng.api.sql_statement_destroy.unwrap())(&mut stmt), ffi::ERROR_NONE);
        assert_eq!((eng.api.statement_iterator_destroy.unwrap())(&mut iter), ffi::ERROR_NONE);
        assert_eq!((eng.api.disconnect.unwrap())(&mut conn), ffi::ERROR_NONE);
        assert_eq!((eng.api.close.unwrap())(&mut db), ffi::ERROR_NONE);
        assert_eq!((eng.api.destroy_environment.unwrap())(&mut env), ffi::ERROR_NONE);
        // The slots must be nulled by the engine — that's spec, not courtesy.
        assert!(env.is_null() && db.is_null() && conn.is_null());
    }
}

#[test]
fn library_reports_a_v2_version() {
    let Some(eng) = v2_engine() else { return };
    assert!(eng.version.starts_with("v2."), "unexpected engine version {}", eng.version);
}
