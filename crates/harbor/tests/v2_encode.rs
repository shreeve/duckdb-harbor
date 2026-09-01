//! v2 encoder coverage: real queries through the whole v2 path — parse,
//! execute, type tree, reader tree, cell emission — pinned against expected
//! wire bytes. The expectations are the 0.20 wire contract (src/encode.rs);
//! where v2 goes beyond it (TIME_NS encodes instead of refusing) the new
//! behavior is pinned here too.
//!
//! Skips (passes vacuously, with a note) when the resolvable engine predates
//! the v2 C API.

use harbor::v2::{Engine, Error, encode, engine, ffi};

/// Run one statement and encode every row: [schema_json, row_json, ...].
fn run(eng: &Engine, sql: &str) -> Result<Vec<String>, Error> {
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
        }
        lines.push(schema);

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
                let mut line = String::new();
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
    Ok(lines)
}

fn v2_engine() -> Option<&'static Engine> {
    match engine() {
        Ok(e) => Some(e),
        Err(e) => {
            eprintln!("v2_encode: skipped — {e}");
            None
        }
    }
}

/// One statement, one row: assert the encoded row (line 1; line 0 is schema).
#[track_caller]
fn row(eng: &Engine, sql: &str, expect: &str) {
    let lines = run(eng, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert_eq!(lines.len(), 2, "{sql}: expected one row, got {}", lines.len() - 1);
    assert_eq!(lines[1], expect, "for {sql}");
}

/// One statement: assert the schema line.
#[track_caller]
fn schema(eng: &Engine, sql: &str, expect: &str) {
    let lines = run(eng, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert_eq!(lines[0], expect, "for {sql}");
}

#[test]
fn integers_and_the_json_safe_rule() {
    let Some(eng) = v2_engine() else { return };
    row(eng, "SELECT true, false, (-128)::TINYINT, 32767::SMALLINT", "true,false,-128,32767");
    row(eng, "SELECT (-2147483648)::INTEGER, 255::UTINYINT, 65535::USMALLINT, 4294967295::UINTEGER", "-2147483648,255,65535,4294967295");
    // BIGINT within 2^53-1 is bare; past it, quoted.
    row(eng, "SELECT 9007199254740991::BIGINT, 9007199254740992::BIGINT", r#"9007199254740991,"9007199254740992""#);
    row(eng, "SELECT 18446744073709551615::UBIGINT", r#""18446744073709551615""#);
    row(eng, "SELECT 42::HUGEINT, 170141183460469231731687303715884105727::HUGEINT", r#"42,"170141183460469231731687303715884105727""#);
    // HUGEINT minimum: the unsigned_abs edge.
    row(eng, "SELECT (-170141183460469231731687303715884105728)::HUGEINT", r#""-170141183460469231731687303715884105728""#);
    row(eng, "SELECT 42::UHUGEINT, 340282366920938463463374607431768211455::UHUGEINT", r#"42,"340282366920938463463374607431768211455""#);
}

#[test]
fn floats() {
    let Some(eng) = v2_engine() else { return };
    row(eng, "SELECT 0.1::FLOAT, 0.1::DOUBLE, 1.5::DOUBLE", "0.1,0.1,1.5");
    row(eng, "SELECT 'NaN'::DOUBLE, 'Infinity'::DOUBLE, '-Infinity'::FLOAT", r#""NaN","Infinity","-Infinity""#);
    row(eng, "SELECT 1e21::DOUBLE, 1e300::DOUBLE", "1e+21,1e+300");
    row(eng, "SELECT 3e38::FLOAT", "3e+38");
}

#[test]
fn text_and_bytes() {
    let Some(eng) = v2_engine() else { return };
    row(eng, r#"SELECT 'hello', 'quote " and \ back', chr(10)"#, r#""hello","quote \" and \\ back","\n""#);
    // A string too long to inline in the 16-byte cell.
    row(eng, "SELECT repeat('ab', 20)", format!("\"{}\"", "ab".repeat(20)).as_str());
    // U+2028 is a line terminator to NDJSON consumers; it must leave escaped.
    row(eng, "SELECT '\u{2028}'", r#""\u2028""#);
    row(eng, "SELECT '\\x01\\x02\\xFF'::BLOB", r#""AQL/""#);
    row(eng, "SELECT '101'::BIT, '10110011101'::BIT", r#""101","10110011101""#);
}

#[test]
fn bignum_small_and_large() {
    let Some(eng) = v2_engine() else { return };
    row(eng, "SELECT 42::BIGNUM, (-42)::BIGNUM", "42,-42");
    row(eng, "SELECT '10000000000000000000000000000000000000000'::BIGNUM", r#""10000000000000000000000000000000000000000""#);
    row(eng, "SELECT '-10000000000000000000000000000000000000000'::BIGNUM", r#""-10000000000000000000000000000000000000000""#);
}

#[test]
fn temporal() {
    let Some(eng) = v2_engine() else { return };
    row(eng, "SELECT DATE '2026-09-01', DATE '1969-07-20'", r#""2026-09-01","1969-07-20""#);
    row(eng, "SELECT TIME '14:30:00', TIME '14:30:00.123456'", r#""14:30:00","14:30:00.123456""#);
    row(eng, "SELECT TIMESTAMP '2026-09-01 14:30:00.5'", r#""2026-09-01T14:30:00.5""#);
    row(eng, "SELECT TIMESTAMP_S '2026-09-01 14:30:00'", r#""2026-09-01T14:30:00""#);
    row(eng, "SELECT TIMESTAMP_MS '2026-09-01 14:30:00.123'", r#""2026-09-01T14:30:00.123""#);
    row(eng, "SELECT TIMESTAMP_NS '2026-09-01 14:30:00.123456789'", r#""2026-09-01T14:30:00.123456789""#);
    row(eng, "SELECT TIMESTAMPTZ '2026-09-01 14:30:00+00'", r#""2026-09-01T14:30:00Z""#);
    row(eng, "SELECT DATE '1600-02-29'", r#""1600-02-29""#);
    // v1 refused TIME_NS outright; v2 encodes it.
    row(eng, "SELECT TIME_NS '14:30:00.123456789'", r#""14:30:00.123456789""#);
    // TIME WITH TIME ZONE: local time kept, offset dropped, schema says so.
    row(eng, "SELECT TIMETZ '14:30:00+02'", r#""14:30:00""#);
    row(eng, "SELECT INTERVAL '1 year 2 days 3 seconds'", r#"{"months":12,"days":2,"micros":"3000000"}"#);
}

#[test]
fn decimals_across_all_four_tiers() {
    let Some(eng) = v2_engine() else { return };
    row(eng, "SELECT 1.23::DECIMAL(4,2), (-1.20)::DECIMAL(4,2)", r#""1.23","-1.20""#);
    row(eng, "SELECT 1234567.89::DECIMAL(9,2)", r#""1234567.89""#);
    row(eng, "SELECT 123456789012345.678::DECIMAL(18,3)", r#""123456789012345.678""#);
    row(eng, "SELECT 12345678901234567890123456789012.345678::DECIMAL(38,6)", r#""12345678901234567890123456789012.345678""#);
    row(eng, "SELECT 5::DECIMAL(3,0), 0.007::DECIMAL(3,3)", r#""5","0.007""#);
}

#[test]
fn uuid_enum_and_null() {
    let Some(eng) = v2_engine() else { return };
    row(eng, "SELECT '550e8400-e29b-41d4-a716-446655440000'::UUID", r#""550e8400-e29b-41d4-a716-446655440000""#);
    row(eng, "SELECT 'b'::ENUM('a','b','c')", r#""b""#);
    row(eng, "SELECT NULL", "null");
    row(eng, "SELECT NULL::INTEGER, NULL::VARCHAR, NULL::DOUBLE", "null,null,null");
}

#[test]
fn nested_types() {
    let Some(eng) = v2_engine() else { return };
    row(eng, "SELECT [1, 2, 3], []::INTEGER[], [NULL::INTEGER, 4]", "[1,2,3],[],[null,4]");
    row(eng, "SELECT [[1],[2,3]]", "[[1],[2,3]]");
    row(eng, "SELECT [1, 2, 3]::INTEGER[3]", "[1,2,3]");
    row(eng, "SELECT {'a': 1, 'b': 'x'}", r#"{"a":1,"b":"x"}"#);
    row(eng, "SELECT {'outer': {'inner': [1,2]}}", r#"{"outer":{"inner":[1,2]}}"#);
    row(eng, "SELECT MAP([1,2],['x','y'])", r#"[[1,"x"],[2,"y"]]"#);
    row(eng, "SELECT MAP()::MAP(INTEGER,VARCHAR)", "[]");
    row(eng, "SELECT NULL::STRUCT(a INTEGER)", "null");
    row(eng, "SELECT [NULL::STRUCT(a INTEGER), {'a': 7}]", r#"[null,{"a":7}]"#);
}

#[test]
fn unions_keep_their_tags_at_the_top() {
    let Some(eng) = v2_engine() else { return };
    row(eng, "SELECT union_value(a := 2)::UNION(a INTEGER, b VARCHAR)", r#"{"tag":"a","value":2}"#);
    row(eng, "SELECT union_value(b := 'x')::UNION(a INTEGER, b VARCHAR)", r#"{"tag":"b","value":"x"}"#);
    // Nested, the wire keeps v1's shape: the payload alone.
    row(eng, "SELECT [union_value(a := 2)::UNION(a INTEGER, b VARCHAR)]", "[2]");
}

#[test]
fn schema_lines() {
    let Some(eng) = v2_engine() else { return };
    schema(eng, "SELECT 1 AS n", r#"{"name":"n","duckdbType":"INTEGER","lossless":true}"#);
    schema(eng, "SELECT 1.5::DECIMAL(4,2) AS d", r#"{"name":"d","duckdbType":"DECIMAL(4,2)","lossless":true,"decimal":{"width":4,"scale":2}}"#);
    schema(eng, "SELECT [1] AS l", r#"{"name":"l","duckdbType":"INTEGER[]","lossless":true,"child":{"duckdbType":"INTEGER","lossless":true}}"#);
    schema(eng, "SELECT [1,2]::INTEGER[2] AS a", r#"{"name":"a","duckdbType":"INTEGER[2]","lossless":true,"arrayLength":2,"child":{"duckdbType":"INTEGER","lossless":true}}"#);
    // A keyword field name is quoted in the type string, as DuckDB itself does.
    schema(eng, "SELECT {'name': 1} AS s", r#"{"name":"s","duckdbType":"STRUCT(\"name\" INTEGER)","lossless":true,"fields":[{"name":"name","duckdbType":"INTEGER","lossless":true}]}"#);
    schema(eng, "SELECT MAP([1],['x']) AS m", r#"{"name":"m","duckdbType":"MAP(INTEGER, VARCHAR)","lossless":true,"keyType":{"duckdbType":"INTEGER","lossless":true},"valueType":{"duckdbType":"VARCHAR","lossless":true},"encoding":"pairs"}"#);
    schema(eng, "SELECT 'a'::ENUM('a','b') AS e", r#"{"name":"e","duckdbType":"ENUM('a', 'b')","lossless":true,"values":["a","b"]}"#);
    schema(eng, "SELECT TIMETZ '14:30:00+02' AS t", r#"{"name":"t","duckdbType":"TIME WITH TIME ZONE","lossless":false,"encoding":"time-offset-dropped"}"#);
    schema(
        eng,
        "SELECT union_value(a := 2)::UNION(a INTEGER, b VARCHAR) AS u",
        r#"{"name":"u","duckdbType":"UNION(a INTEGER, b VARCHAR)","lossless":true,"members":[{"name":"a","duckdbType":"INTEGER","lossless":true},{"name":"b","duckdbType":"VARCHAR","lossless":true}]}"#,
    );
    schema(
        eng,
        "SELECT [union_value(a := 2)::UNION(a INTEGER, b VARCHAR)] AS lu",
        r#"{"name":"lu","duckdbType":"UNION(a INTEGER, b VARCHAR)[]","lossless":true,"child":{"duckdbType":"UNION(a INTEGER, b VARCHAR)","lossless":false,"encoding":"union-tag-dropped","members":[{"name":"a","duckdbType":"INTEGER","lossless":true},{"name":"b","duckdbType":"VARCHAR","lossless":true}]}}"#,
    );
    schema(eng, "SELECT NULL AS x", r#"{"name":"x","duckdbType":"\"NULL\"","lossless":true}"#);
    schema(eng, "SELECT TIME_NS '14:30:00' AS t", r#"{"name":"t","duckdbType":"TIME_NS","lossless":true}"#);
}

#[test]
fn vector_representations() {
    let Some(eng) = v2_engine() else { return };
    // A constant vector: one value repeated across the chunk.
    let lines = run(eng, "SELECT 42 AS c, 'x' AS s FROM range(4)").unwrap();
    assert_eq!(&lines[1..], &["42,\"x\""; 4]);
    // NULL constant beside data.
    let lines = run(eng, "SELECT NULL::INTEGER AS n, range AS r FROM range(3)").unwrap();
    assert_eq!(&lines[1..], ["null,0", "null,1", "null,2"]);
    // A dictionary-shaped result (DuckDB dictionary-encodes many filters).
    let lines = run(eng, "SELECT range % 3 AS m FROM range(10) WHERE range % 2 = 0").unwrap();
    assert_eq!(&lines[1..], ["0", "2", "1", "0", "2"]);
    // More rows than one chunk holds: 3000 crosses the 2048 boundary.
    let lines = run(eng, "SELECT sum(range::BIGINT) OVER () AS s FROM range(3000)").unwrap();
    assert_eq!(lines.len(), 3001);
    assert!(lines[1..].iter().all(|l| l == "4498500"));
}

#[test]
fn multi_statement_and_tables_round_trip() {
    let Some(eng) = v2_engine() else { return };
    let lines = run(
        eng,
        "CREATE TABLE t(a INTEGER, b VARCHAR); INSERT INTO t VALUES (1,'x'),(2,NULL); SELECT * FROM t ORDER BY a",
    )
    .unwrap();
    let rows: Vec<&str> = lines.iter().map(|s| s.as_str()).filter(|l| !l.starts_with('{')).collect();
    // CREATE emits a success row, INSERT a count row, then the data.
    assert!(rows.contains(&r#"1,"x""#));
    assert!(rows.contains(&"2,null"));
}

#[test]
fn tuples_and_variants() {
    let Some(eng) = v2_engine() else { return };
    // TUPLE: unnamed, so values travel as an array — an object would collide
    // on the empty key — and the type string is the engine's own spelling.
    row(eng, "SELECT ROW(1, 'a'), (2,)", r#"[1,"a"],[2]"#);
    schema(
        eng,
        "SELECT ROW(1, 'a') AS t",
        r#"{"name":"t","duckdbType":"TUPLE(INTEGER, VARCHAR)","lossless":true,"fields":[{"duckdbType":"INTEGER","lossless":true},{"duckdbType":"VARCHAR","lossless":true}]}"#,
    );
    // VARIANT has no committed view layout; it goes out as the engine's text
    // rendering, and the schema says the payload is a cast, not the value.
    row(eng, "SELECT 42::VARIANT, {'a': 1}::VARIANT", r#""42","{'a': 1}""#);
    schema(eng, "SELECT 42::VARIANT AS v", r#"{"name":"v","duckdbType":"VARIANT","lossless":false,"encoding":"varchar-cast"}"#);
}
