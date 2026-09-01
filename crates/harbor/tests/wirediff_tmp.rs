//! TEMPORARY differential harness — DELETE AFTER USE.
//!
//! Reads /tmp/wirediff_queries.txt (one SQL statement per line), runs each
//! through the v2 encode path, and prints delimited lines for an external
//! diff against the v1 server's wire output:
//!   #Q<i>|<sql>
//!   S|<schema line bytes>   (the columns array content)
//!   R|<row line bytes>      (the values array content)
//!   E|<error>               (if the statement failed)
//!   A|<i>|<alias>           (alias probe; ignored by the differ)

use harbor::v2::{Engine, Error, encode, engine, ffi};

fn run(eng: &Engine, sql: &str) -> Result<(Vec<String>, Vec<String>), Error> {
    let api = &eng.api;

    macro_rules! ok {
        ($f:ident($($a:expr),*)) => {{
            let mut err: ffi::error_info_handle = std::ptr::null_mut();
            let code = unsafe { (api.$f.expect(stringify!($f)))($($a,)* &mut err) };
            if code != ffi::ERROR_NONE {
                return Err(Error::take(api, code, err));
            }
        }};
    }

    let mut env: ffi::environment_handle = std::ptr::null_mut();
    ok!(create_environment(&mut env));
    let mut db: ffi::database_handle = std::ptr::null_mut();
    ok!(open(env, ffi::str_t { ptr: std::ptr::null(), len: 0 }, std::ptr::null_mut(), 0, &mut db));
    let mut conn: ffi::connection_handle = std::ptr::null_mut();
    ok!(connect(db, &mut conn));

    let sql_c = std::ffi::CString::new(sql).unwrap();
    let mut iter: ffi::statement_iterator_handle = std::ptr::null_mut();
    ok!(parse_sql(conn, sql_c.as_ptr(), &mut iter));

    let mut lines = Vec::new();
    let mut aliases = Vec::new();
    loop {
        let mut stmt: ffi::sql_statement_handle = std::ptr::null_mut();
        ok!(statement_iterator_next(iter, &mut stmt));
        if stmt.is_null() {
            break;
        }
        let mut result: ffi::result_handle = std::ptr::null_mut();
        ok!(statement_execute(conn, stmt, std::ptr::null(), std::ptr::null(), 0, &mut result));

        let columns = encode::result_columns(api, result)?;
        let mut schema = String::new();
        for (i, (name, ty)) in columns.iter().enumerate() {
            if i > 0 {
                schema.push(',');
            }
            encode::emit_column_schema(&mut schema, Some(name), ty);
            if let Some(a) = &ty.alias {
                aliases.push(format!("col {i}: alias={a:?}"));
            }
        }
        lines.push(format!("S|{schema}"));

        loop {
            let mut chunk: ffi::data_chunk_handle = std::ptr::null_mut();
            ok!(result_fetch_chunk(result, &mut chunk));
            if chunk.is_null() {
                break;
            }
            let mut rows: ffi::idx_t = 0;
            ok!(data_chunk_get_size(chunk, &mut rows));
            let mut readers = Vec::with_capacity(columns.len());
            for i in 0..columns.len() {
                let mut vector: ffi::vector_handle = std::ptr::null_mut();
                ok!(data_chunk_get_vector(chunk, i as ffi::idx_t, &mut vector));
                readers.push(encode::Reader::of(api, vector)?);
            }
            for row in 0..rows as usize {
                let mut line = String::from("R|");
                for (i, (_, ty)) in columns.iter().enumerate() {
                    if i > 0 {
                        line.push(',');
                    }
                    encode::emit_cell(&mut line, api, &readers[i], ty, row)?;
                }
                lines.push(line);
            }
            unsafe { (api.data_chunk_destroy.unwrap())(&mut chunk) };
        }
        unsafe {
            (api.result_destroy.unwrap())(&mut result);
            (api.sql_statement_destroy.unwrap())(&mut stmt);
        }
    }

    unsafe {
        (api.statement_iterator_destroy.unwrap())(&mut iter);
        (api.disconnect.unwrap())(&mut conn);
        (api.close.unwrap())(&mut db);
        (api.destroy_environment.unwrap())(&mut env);
    }
    Ok((lines, aliases))
}

#[test]
fn wirediff_dump() {
    let eng = match engine() {
        Ok(e) => e,
        Err(e) => {
            println!("E|engine unavailable: {e}");
            return;
        }
    };
    let queries = std::fs::read_to_string("/tmp/wirediff_queries.txt").expect("queries file");
    for (i, sql) in queries.lines().enumerate() {
        let sql = sql.trim();
        if sql.is_empty() || sql.starts_with('#') {
            continue;
        }
        println!("#Q{i}|{sql}");
        match run(eng, sql) {
            Ok((lines, aliases)) => {
                for l in &lines {
                    println!("{l}");
                }
                for a in &aliases {
                    println!("A|{i}|{a}");
                }
            }
            Err(e) => println!("E|{e}"),
        }
    }
}
