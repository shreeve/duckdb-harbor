//! Probes against the machine's real fleet. Ignored by default: they need a
//! live Harbor and say nothing in CI. Run explicitly:
//! `cargo test -p harbor-client --test live -- --ignored --nocapture`

use harbor_client::{connect, fleet, info};

/// Any LOCAL berth will do — a name is a service that starts on use, so a
/// stopped configured berth is as connectable as a live one. Live still wins,
/// to avoid churning starts when something is already up. A remote is never
/// chosen: it has no local database file, connecting to it opens an SSH
/// tunnel to another machine, and these probes create and drop tables.
fn connectable() -> Option<fleet::Survey> {
    let mut rows: Vec<_> = fleet::survey().rows.into_iter().filter(|r| r.path.is_some()).collect();
    rows.sort_by_key(|r| !r.state.is_live());
    rows.into_iter().next()
}

#[test]
#[ignore]
fn the_real_fleet_lists_and_answers() {
    let fleet = fleet::survey();
    if let Some(w) = &fleet.warning {
        println!("warning: {w}");
    }
    println!("fleet:");
    for row in &fleet.rows {
        println!("  {} {}", row.state.label(), row.name);
        if let Some(note) = &row.note {
            println!("    note: {note}");
        }
    }
    assert!(!fleet.rows.is_empty(), "no berths known to config or runtime");
}

#[test]
#[ignore]
fn a_live_berth_yields_identity() {
    let Some(row) = connectable() else {
        println!("no berth to test against; skipping");
        return;
    };
    let conn = connect(&row.name).expect("connect");
    let identity = info(&conn).expect("info");
    println!(
        "{}: duckdb {} harbor {} db {}",
        identity.name, identity.duckdb_version, identity.harbor_version, identity.database
    );
    assert_eq!(identity.name, row.name);
}

#[test]
#[ignore]
fn a_live_berth_answers_sql() {
    let Some(row) = connectable() else {
        println!("no berth to test against; skipping");
        return;
    };
    let conn = connect(&row.name).expect("connect");
    let result = harbor_client::query(&conn, "SELECT 1 AS one, 'two' AS two, NULL AS three")
        .expect("query");
    println!(
        "columns: {:?}",
        result.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>()
    );
    println!("rows: {:?} ({} in {} ms)", result.rows, result.row_count, result.time_ms);
    assert_eq!(result.columns.len(), 3);
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], serde_json::json!(1));
    assert_eq!(result.rows[0][2], serde_json::Value::Null);
}

/// The File→Open lifetime regression: with a sub-second Harbor linger, pause
/// well beyond zero clients and prove the same DuckTable connection still
/// answers. Run with:
/// `HARBOR_BIN="$(pwd)/../harbor/target/debug/harbor" HARBOR_FIXTURE="$(pwd)/../harbor/sample.duckdb" HARBOR_LINGER_MS=500 cargo test -p harbor-client --test live an_open_database_outlives_harbors_linger -- --ignored`
#[test]
#[ignore]
fn an_open_database_outlives_harbors_linger() {
    let fixture = std::env::var("HARBOR_FIXTURE").expect("set HARBOR_FIXTURE");
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let db = std::env::temp_dir().join(format!("ducktable-anchor-{unique}.duckdb"));
    std::fs::copy(fixture, &db).expect("copy fixture");
    let socket = harbor_common::paths::socket_for(
        &harbor_common::paths::runtime_dir().expect("runtime dir"),
        &db,
    )
    .expect("socket path");

    let conn = fleet::connect_path(&db).expect("connect_path");
    harbor_client::query(&conn, "SELECT 1").expect("first query");
    std::thread::sleep(std::time::Duration::from_secs(2));
    harbor_client::query(&conn, "SELECT 2").expect("query after Harbor linger");
    drop(conn);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while socket.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(!socket.exists(), "ephemeral Harbor stayed after its connection closed");
    let _ = std::fs::remove_file(db);
}

/// The exact wire sequence the GUI's ⌘S performs (docs/EDITING.md):
/// a session pins one connection, BEGIN..COMMIT spans requests on it,
/// parameters bind instead of concatenating, and UPDATE answers with a
/// count row — the affected-exactly-one verification reads that cell.
#[test]
#[ignore]
fn a_session_carries_a_transaction_with_bound_params() {
    use serde_json::json;
    let Some(row) = connectable() else {
        println!("no berth to test against; skipping");
        return;
    };
    let conn = connect(&row.name).expect("connect");
    let sid = harbor_client::session_new(&conn).expect("session");
    let run = |sql: &str, params: Option<Vec<serde_json::Value>>| {
        harbor_client::exec(&conn, sql, params, Some(&sid)).expect(sql)
    };
    // TEMP tables stay on the pinned worker rather than entering the
    // database's real schema. A released session returns that connection
    // to Harbor's pool, so clean up names explicitly for repeatability.
    run("DROP TABLE IF EXISTS _dt_edit_probe", None);
    run("DROP TABLE IF EXISTS _dt_insert_probe", None);
    run("DROP TABLE IF EXISTS _dt_default_probe", None);
    run("CREATE TEMP TABLE _dt_edit_probe(id INTEGER PRIMARY KEY, name VARCHAR)", None);
    run("INSERT INTO _dt_edit_probe VALUES (?, ?), (?, ?)",
        Some(vec![json!(1), json!("a"), json!(2), json!("b")]));
    // The staged-update shape: SET by param, WHERE binds the ORIGINAL key.
    run("BEGIN", None);
    let hit = run("UPDATE _dt_edit_probe SET \"name\" = ? WHERE \"id\" = ?",
        Some(vec![json!("z"), json!(1)]));
    println!("update answered: {:?}", hit.rows);
    assert_eq!(hit.rows[0][0].as_u64(), Some(1), "one row, exactly");
    // A WHERE that no longer matches answers 0 — the signal the commit
    // guard turns into a full rollback.
    let miss = run("UPDATE _dt_edit_probe SET \"name\" = ? WHERE \"id\" = ?",
        Some(vec![json!("q"), json!(99)]));
    assert_eq!(miss.rows[0][0].as_u64(), Some(0), "a vanished row answers 0");
    run("COMMIT", None);
    let after = run("SELECT name FROM _dt_edit_probe ORDER BY id", None);
    assert_eq!(after.rows[0][0], json!("z"));
    // And the rollback leg: BEGIN, change, ROLLBACK — nothing landed.
    run("BEGIN", None);
    run("UPDATE _dt_edit_probe SET name = ? WHERE id = ?", Some(vec![json!("gone"), json!(2)]));
    run("ROLLBACK", None);
    let intact = run("SELECT name FROM _dt_edit_probe WHERE id = 2", None);
    assert_eq!(intact.rows[0][0], json!("b"), "rollback left the row untouched");

    // The staged-INSERT shape: omitted columns keep DEFAULT semantics,
    // values bind, and RETURNING exposes the engine-computed truth.
    run(
        "CREATE TEMP TABLE _dt_insert_probe(\
         id INTEGER PRIMARY KEY DEFAULT 41, \
         name VARCHAR NOT NULL, \
         doubled INTEGER GENERATED ALWAYS AS (id * 2))",
        None,
    );
    run("BEGIN", None);
    let inserted = run(
        "INSERT INTO _dt_insert_probe (name) VALUES (?) RETURNING *",
        Some(vec![json!("Ada")]),
    );
    assert_eq!(inserted.rows, vec![vec![json!(41), json!("Ada"), json!(82)]]);
    run("COMMIT", None);
    run("CREATE TEMP TABLE _dt_default_probe(answer INTEGER DEFAULT 42)", None);
    let defaults = run("INSERT INTO _dt_default_probe DEFAULT VALUES RETURNING *", None);
    assert_eq!(defaults.rows, vec![vec![json!(42)]]);
    run("DROP TABLE _dt_edit_probe", None);
    run("DROP TABLE _dt_insert_probe", None);
    run("DROP TABLE _dt_default_probe", None);
    harbor_client::session_release(&conn, &sid);
    println!("session {sid}: transaction, params, counts — all as the spec assumes");
}

#[test]
#[ignore]
fn keyless_base_tables_expose_rowid() {
    let Some(row) = connectable() else {
        println!("no berth to test against; skipping");
        return;
    };
    let conn = connect(&row.name).expect("connect");
    // Find any base table, then probe the rowid pseudocolumn through
    // the same wire the grid would use.
    let tables = harbor_client::query(
        &conn,
        "SELECT schema_name, table_name FROM duckdb_tables() LIMIT 1",
    )
    .expect("duckdb_tables");
    let Some(t) = tables.rows.first() else {
        println!("no base tables; skipping");
        return;
    };
    let (schema, name) = (t[0].as_str().unwrap(), t[1].as_str().unwrap());
    let sql = format!(
        "SELECT [rowid::UBIGINT, hash(*COLUMNS(*))] AS rowid, * FROM \"{schema}\".\"{name}\" LIMIT 3"
    );
    let result = harbor_client::query(&conn, &sql).expect("rowid probe");
    println!("rowid probe on {schema}.{name}:");
    println!("  columns: {:?}", result.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>());
    for r in &result.rows {
        println!("  {:?}", r.first());
    }
    assert!(result.columns.first().and_then(|c| c.name.as_deref()) == Some("rowid"));
    // And the shape an UPDATE would use: the rowid and the aliased row's
    // hash in the WHERE.
    let sql = format!(
        "SELECT count(*) FROM \"{schema}\".\"{name}\" AS \"row\" WHERE \"rowid\" = 0 AND hash(\"row\") = 0::UBIGINT"
    );
    let count = harbor_client::query(&conn, &sql).expect("rowid and hash in WHERE");
    println!("  quoted-WHERE count row: {:?}", count.rows.first());
}

/// The placeholders DuckTable binds a VARIANT, a JSON and a BLOB cell
/// through (ducktable's `edits.rs`), against the engine, in the session
/// transaction a commit runs in. A bare `?` stores a VARIANT string whose
/// every path is NULL, and stores a BLOB's base64 characters as its bytes;
/// this is the proof that the typed forms do not.
#[test]
#[ignore]
fn document_and_blob_cells_bind_as_what_they_are() {
    use serde_json::json;
    let Some(row) = connectable() else {
        println!("no berth to test against; skipping");
        return;
    };
    let conn = connect(&row.name).expect("connect");
    let sid = harbor_client::session_new(&conn).expect("session");
    let run = |sql: &str, params: Option<Vec<serde_json::Value>>| {
        harbor_client::exec(&conn, sql, params, Some(&sid)).expect(sql)
    };
    run("DROP TABLE IF EXISTS _dt_typed_probe", None);
    run("DROP TABLE IF EXISTS _dt_blobkey_probe", None);
    run("CREATE TEMP TABLE _dt_typed_probe(id INTEGER PRIMARY KEY, doc VARIANT, j JSON, b BLOB)", None);
    run("INSERT INTO _dt_typed_probe VALUES (1, '{\"a\":{\"b\":1}}'::JSON, '[1]', '\\xAA\\xBB'::BLOB)", None);

    run("BEGIN", None);
    let hit = run(
        "UPDATE _dt_typed_probe SET \"doc\" = ?::JSON, \"j\" = ?::JSON, \"b\" = from_base64(?::VARCHAR) WHERE \"id\" = ?",
        Some(vec![json!("{\"a\":{\"b\":2}}"), json!("{\"k\": null}"), json!("AAECAw=="), json!(1)]),
    );
    assert_eq!(hit.rows[0][0].as_u64(), Some(1), "one row, exactly");
    // The draft-row shape, with the NULLs a cleared cell binds.
    let made = run(
        "INSERT INTO _dt_typed_probe (\"id\", \"doc\", \"j\", \"b\") VALUES (?, ?::JSON, ?::JSON, from_base64(?::VARCHAR)) RETURNING *",
        Some(vec![json!(2), serde_json::Value::Null, serde_json::Value::Null, serde_json::Value::Null]),
    );
    assert_eq!(made.rows.len(), 1, "RETURNING answers the one row");
    // A quoted string is a string, a bare number a number.
    run(
        "INSERT INTO _dt_typed_probe (\"id\", \"doc\") VALUES (?, ?::JSON), (?, ?::JSON)",
        Some(vec![json!(3), json!("\"Morel\""), json!(4), json!("42")]),
    );
    run("COMMIT", None);

    let after = run(
        "SELECT id, variant_typeof(doc), doc.a.b::VARCHAR, doc IS NULL, j, j IS NULL, \
         octet_length(b), b = '\\x00\\x01\\x02\\x03'::BLOB, b IS NULL \
         FROM _dt_typed_probe ORDER BY id",
        None,
    );
    println!("typed cells: {:?}", after.rows);
    assert_eq!(after.rows[0][1], json!("OBJECT(a)"), "a document, not a string");
    assert_eq!(after.rows[0][2], json!("2"), "and its paths read");
    assert_eq!(after.rows[0][4], json!("{\"k\": null}"), "a JSON column keeps its text");
    assert_eq!(after.rows[0][6].as_u64(), Some(4), "four bytes, not eight base64 characters");
    assert_eq!(after.rows[0][7], json!(true));
    assert_eq!(after.rows[1][3], json!(true), "a NULL param through ?::JSON is SQL NULL");
    assert_eq!(after.rows[1][5], json!(true));
    assert_eq!(after.rows[1][8], json!(true), "and through from_base64(?::VARCHAR)");
    assert_eq!(after.rows[2][1], json!("VARCHAR"));
    assert_eq!(after.rows[3][1], json!("UINT64"));

    // A BLOB key: the WHERE decodes it too, so the row named is the row hit.
    // Row B's bytes are the characters of row A's base64.
    run("CREATE TEMP TABLE _dt_blobkey_probe(k BLOB PRIMARY KEY, name VARCHAR)", None);
    run("INSERT INTO _dt_blobkey_probe VALUES ('\\x00\\x01'::BLOB, 'A'), ('AAE='::BLOB, 'B')", None);
    let hit = run(
        "UPDATE _dt_blobkey_probe SET \"name\" = ? WHERE \"k\" = from_base64(?::VARCHAR)",
        Some(vec![json!("hit"), json!("AAE=")]),
    );
    assert_eq!(hit.rows[0][0].as_u64(), Some(1));
    let names = run("SELECT name FROM _dt_blobkey_probe ORDER BY octet_length(k)", None);
    assert_eq!(names.rows[0][0], json!("hit"), "row A, the two bytes");
    assert_eq!(names.rows[1][0], json!("B"), "row B untouched");

    run("DROP TABLE _dt_typed_probe", None);
    run("DROP TABLE _dt_blobkey_probe", None);
}

/// The statement Duplicate Row commits (ducktable's `edits.rs`): untouched
/// cells are selected from the source row, a typed-over cell is bound, and
/// the WHERE names the source by its key or its rowid. The wire is narrower
/// than the engine — a VARIANT crosses it as JSON, a BLOB[] as base64 text,
/// an INTERVAL as an object, a MAP as pairs — so a copy that rebinds what
/// was fetched is a different value; this is the proof that the copy made
/// in SQL is not, in the session transaction a commit runs in.
#[test]
#[ignore]
fn a_duplicate_row_copies_in_sql_what_the_wire_cannot_carry() {
    use serde_json::json;
    let Some(row) = connectable() else {
        println!("no berth to test against; skipping");
        return;
    };
    let conn = connect(&row.name).expect("connect");
    let sid = harbor_client::session_new(&conn).expect("session");
    let run = |sql: &str, params: Option<Vec<serde_json::Value>>| {
        harbor_client::exec(&conn, sql, params, Some(&sid)).expect(sql)
    };
    run("DROP TABLE IF EXISTS _dt_dup_probe", None);
    run("DROP TABLE IF EXISTS _dt_dup_keyless_probe", None);
    run("DROP SEQUENCE IF EXISTS _dt_dup_seq", None);
    run("CREATE TEMP SEQUENCE _dt_dup_seq START 100", None);
    run(
        "CREATE TEMP TABLE _dt_dup_probe(id INTEGER PRIMARY KEY DEFAULT nextval('_dt_dup_seq'), \
         name VARCHAR, doc VARIANT, bl BLOB[], iv INTERVAL, m MAP(VARCHAR, INTEGER), \
         u UNION(n INTEGER, s VARCHAR), docs VARIANT[])",
        None,
    );
    run(
        "INSERT INTO _dt_dup_probe VALUES (1, 'source', \
         {'day': DATE '2024-02-29', 'big': 170141183460469231731687303715884105727::HUGEINT}::VARIANT, \
         ['hi'::BLOB, '\\xFF'::BLOB], INTERVAL '14 months 2 days 3 seconds', MAP {'a': 1, 'b': 2}, \
         union_value(s := '7'), [DATE '2024-02-29'::VARIANT, 12.340::DECIMAL(10,3)::VARIANT])",
        None,
    );

    // What the grid fetched, and what rebinding it makes of the document.
    let wire = run("SELECT doc, bl, iv, m, u, docs FROM _dt_dup_probe WHERE id = 1", None);
    println!("the source row on the wire: {:?}", wire.rows[0]);

    run("BEGIN", None);
    let rebound = run(
        "INSERT INTO _dt_dup_probe (\"name\", \"doc\") VALUES (?, ?::JSON) RETURNING id",
        Some(vec![json!("rebound"), wire.rows[0][0].clone()]),
    );
    // The duplicate: `name` typed over, everything else read from the source.
    let copied = run(
        "INSERT INTO _dt_dup_probe (\"name\", \"doc\", \"bl\", \"iv\", \"m\", \"u\", \"docs\") \
         SELECT ?, \"doc\", \"bl\", \"iv\", \"m\", \"u\", \"docs\" FROM _dt_dup_probe WHERE \"id\" = ? RETURNING *",
        Some(vec![json!("copy"), json!(1)]),
    );
    assert_eq!(copied.rows.len(), 1, "RETURNING answers the one row");
    assert_eq!(copied.rows[0][1], json!("copy"), "the bound cell is the typed one");
    // A source row that is gone returns nothing, which commit refuses; a
    // scalar subquery in VALUES would insert a row of NULLs instead.
    let gone = run(
        "INSERT INTO _dt_dup_probe (\"name\", \"doc\") SELECT ?, \"doc\" FROM _dt_dup_probe WHERE \"id\" = ? RETURNING *",
        Some(vec![json!("orphan"), json!(999)]),
    );
    assert!(gone.rows.is_empty(), "no source row, no insert");
    let nulls = run(
        "INSERT INTO _dt_dup_probe (\"name\", \"doc\") VALUES (?, (SELECT \"doc\" FROM _dt_dup_probe WHERE \"id\" = ?)) RETURNING doc IS NULL",
        Some(vec![json!("orphan"), json!(999)]),
    );
    println!("a gone source: selected from, {:?}; as a scalar subquery, {:?}", gone.rows, nulls.rows);
    assert_eq!(nulls.rows, vec![vec![json!(true)]], "the shape not used lands a NULL and says nothing");

    let compare = |id: &serde_json::Value| {
        run(
            "SELECT c.name, c.doc = s.doc, variant_typeof(c.doc.day), variant_typeof(c.doc.big), \
             c.bl IS NOT DISTINCT FROM s.bl, c.iv IS NOT DISTINCT FROM s.iv, \
             c.m IS NOT DISTINCT FROM s.m, c.u IS NOT DISTINCT FROM s.u, \
             c.docs IS NOT DISTINCT FROM s.docs, variant_typeof(c.docs[1]), variant_typeof(c.docs[2]) \
             FROM _dt_dup_probe c, _dt_dup_probe s WHERE s.id = 1 AND c.id = ?",
            Some(vec![id.clone()]),
        )
        .rows
        .remove(0)
    };
    let source = compare(&json!(1));
    let rebound = compare(&rebound.rows[0][0]);
    let copy = compare(&copied.rows[0][0]);
    println!("source:  {source:?}");
    println!("rebound: {rebound:?}");
    println!("copy:    {copy:?}");
    assert_eq!(source[2], json!("DATE"));
    assert_eq!(source[3], json!("INT128"));
    assert_eq!(rebound[2], json!("VARCHAR"), "JSON has no DATE, so the wire's copy is a string");
    assert_ne!(rebound[3], source[3], "nor a 128-bit integer");
    assert_eq!(copy[1..], source[1..], "the copy made in SQL is the source, type for type");
    assert!(copy[1..].iter().all(|v| v.as_bool() != Some(false)));

    // The source row is deleted in the same transaction, after the insert
    // read it: deletes run last.
    let hit = run("DELETE FROM _dt_dup_probe WHERE \"id\" = ?", Some(vec![json!(1)]));
    assert_eq!(hit.rows[0][0].as_u64(), Some(1));
    run("COMMIT", None);
    let kept = run(
        "SELECT name, variant_typeof(doc.day), doc.big::VARCHAR, iv::VARCHAR, m::VARCHAR, \
         union_tag(u)::VARCHAR, octet_length(bl[2]) FROM _dt_dup_probe WHERE name = 'copy'",
        None,
    );
    println!("the copy, its source deleted: {:?}", kept.rows);
    assert_eq!(
        kept.rows,
        vec![vec![
            json!("copy"),
            json!("DATE"),
            json!("170141183460469231731687303715884105727"),
            json!("1 year 2 months 2 days 00:00:03"),
            json!("{a=1, b=2}"),
            json!("s"),
            json!(1),
        ]]
    );

    // A keyless table names the source by its rowid.
    run("CREATE TEMP TABLE _dt_dup_keyless_probe(name VARCHAR, doc VARIANT)", None);
    run("INSERT INTO _dt_dup_keyless_probe VALUES ('a', DATE '2020-01-01'::VARIANT)", None);
    let source_rowid = run("SELECT rowid FROM _dt_dup_keyless_probe", None).rows.remove(0).remove(0);
    let copied = run(
        "INSERT INTO _dt_dup_keyless_probe (\"name\", \"doc\") SELECT \"name\", \"doc\" \
         FROM _dt_dup_keyless_probe WHERE \"rowid\" = ? RETURNING *",
        Some(vec![source_rowid]),
    );
    assert_eq!(copied.rows.len(), 1);
    let types = run("SELECT name, variant_typeof(doc) FROM _dt_dup_keyless_probe ORDER BY rowid", None);
    println!("keyless, by rowid: {:?}", types.rows);
    assert_eq!(types.rows[0], types.rows[1]);
    assert_eq!(types.rows[1][1], json!("DATE"));

    run("DROP TABLE _dt_dup_probe", None);
    run("DROP TABLE _dt_dup_keyless_probe", None);
    run("DROP SEQUENCE _dt_dup_seq", None);
}

/// What DuckTable's `parse_value` binds for a number JSON cannot carry: an
/// integer past 64 bits as its digits, a DOUBLE that is not finite by name.
/// The engine casts both exactly, and refuses `''` for an ENUM and a UUID,
/// which is why neither clears to the empty string.
#[test]
#[ignore]
fn wide_integers_and_non_finite_doubles_bind_as_text() {
    use serde_json::json;
    let Some(row) = connectable() else {
        println!("no berth to test against; skipping");
        return;
    };
    let conn = connect(&row.name).expect("connect");
    let sid = harbor_client::session_new(&conn).expect("session");
    let run = |sql: &str, params: Option<Vec<serde_json::Value>>| {
        harbor_client::exec(&conn, sql, params, Some(&sid)).expect(sql)
    };
    run("DROP TABLE IF EXISTS _dt_number_probe", None);
    run(
        "CREATE TEMP TABLE _dt_number_probe(k INTEGER, h HUGEINT, ub UBIGINT, uh UHUGEINT, \
         d DOUBLE, f FLOAT, e ENUM('POINT', 'LINE'), u UUID)",
        None,
    );
    let wide = [
        "170141183460469231731687303715884105727",
        "18446744073709551615",
        "340282366920938463463374607431768211455",
    ];
    let stored = run(
        "INSERT INTO _dt_number_probe (k, h, ub, uh) VALUES (?, ?, ?, ?) \
         RETURNING h::VARCHAR, ub::VARCHAR, uh::VARCHAR",
        Some(vec![json!(1), json!(wide[0]), json!(wide[1]), json!(wide[2])]),
    );
    println!("wide integers bound as text: {:?}", stored.rows[0]);
    assert_eq!(stored.rows[0], wide.map(|w| json!(w)).to_vec());
    let low = run(
        "UPDATE _dt_number_probe SET \"h\" = ? WHERE \"k\" = ?",
        Some(vec![json!("-170141183460469231731687303715884105728"), json!(1)]),
    );
    assert_eq!(low.rows[0][0].as_u64(), Some(1));

    for (name, shown, nan, inf) in [
        ("nan", "NaN", true, false),
        ("NaN", "NaN", true, false),
        ("inf", "Infinity", false, true),
        ("+inf", "Infinity", false, true),
        ("-inf", "-Infinity", false, true),
        ("Infinity", "Infinity", false, true),
        ("-Infinity", "-Infinity", false, true),
        ("infinity", "Infinity", false, true),
    ] {
        let r = run(
            "INSERT INTO _dt_number_probe (k, d, f) VALUES (?, ?, ?) RETURNING d, f, isnan(d), isinf(d), d IS NULL",
            Some(vec![json!(2), json!(name), json!(name)]),
        );
        println!("{name:?} into DOUBLE and FLOAT: {:?}", r.rows[0]);
        assert_eq!(r.rows[0], vec![json!(shown), json!(shown), json!(nan), json!(inf), json!(false)]);
    }

    for (col, ty) in [("e", "ENUM"), ("u", "UUID")] {
        let sql = format!("INSERT INTO _dt_number_probe (k, {col}) VALUES (?, ?)");
        let refused = harbor_client::exec(&conn, &sql, Some(vec![json!(3), json!("")]), Some(&sid));
        println!("'' into {ty}: {:?}", refused.as_ref().err());
        assert!(refused.is_err(), "'' is not a value of {ty}");
    }
    run("DROP TABLE _dt_number_probe", None);
}

/// A FLOAT key in the WHERE (ducktable's `edits.rs`, `key_placeholder_for`).
/// The wire carries a FLOAT as the shortest decimal that names it, and a JSON
/// number binds as a DOUBLE: compared bare, the key is widened and 1.1 the
/// FLOAT is not 1.1 the DOUBLE, so the statement names no row. Cast to FLOAT,
/// the param is the key. The UPDATE, the DELETE and the duplicate's INSERT
/// each name exactly one row, in the session transaction a commit runs in.
#[test]
#[ignore]
fn a_float_key_names_its_row_through_a_cast() {
    use serde_json::json;
    let Some(row) = connectable() else {
        println!("no berth to test against; skipping");
        return;
    };
    let conn = connect(&row.name).expect("connect");
    let sid = harbor_client::session_new(&conn).expect("session");
    let run = |sql: &str, params: Option<Vec<serde_json::Value>>| {
        harbor_client::exec(&conn, sql, params, Some(&sid)).expect(sql)
    };
    run("DROP TABLE IF EXISTS _dt_floatkey_probe", None);
    run("CREATE TEMP TABLE _dt_floatkey_probe(k FLOAT PRIMARY KEY, name VARCHAR, f FLOAT)", None);
    run("INSERT INTO _dt_floatkey_probe VALUES (0.1, 'a', 0.1), (1.1, 'b', 1.1), (0.5, 'c', 0.5)", None);

    // The keys as the grid fetches them: the identity a statement binds.
    let fetched = run("SELECT k FROM _dt_floatkey_probe ORDER BY name", None);
    println!(
        "column type {:?}, keys on the wire {:?}",
        fetched.columns[0].duckdb_type, fetched.rows
    );
    assert_eq!(fetched.columns[0].duckdb_type, "FLOAT");
    assert_eq!(fetched.rows, vec![vec![json!(0.1)], vec![json!(1.1)], vec![json!(0.5)]]);

    run("BEGIN", None);
    for key in fetched.rows.iter().map(|r| r[0].clone()) {
        let bare = run(
            "SELECT count(*) FROM _dt_floatkey_probe WHERE \"k\" = ?",
            Some(vec![key.clone()]),
        );
        let hit = run(
            "UPDATE _dt_floatkey_probe SET \"name\" = ?, \"f\" = ? WHERE \"k\" = ?::FLOAT",
            Some(vec![json!("hit"), json!(2.2), key.clone()]),
        );
        let copied = run(
            "INSERT INTO _dt_floatkey_probe (\"k\", \"name\", \"f\") SELECT ?, \"name\", \"f\" \
             FROM _dt_floatkey_probe WHERE \"k\" = ?::FLOAT RETURNING k, f",
            Some(vec![json!(key.as_f64().unwrap() + 10.0), key.clone()]),
        );
        println!(
            "key {key}: bare `?` matches {}, `?::FLOAT` updates {}, the duplicate returns {:?}",
            bare.rows[0][0], hit.rows[0][0], copied.rows
        );
        // 0.5 is the same number in both widths; 0.1 and 1.1 are not.
        let exact = key == json!(0.5);
        assert_eq!(bare.rows[0][0].as_u64(), Some(exact as u64), "{key} compared as a DOUBLE");
        assert_eq!(hit.rows[0][0].as_u64(), Some(1), "{key}: one row, exactly");
        assert_eq!(copied.rows.len(), 1, "{key}: the duplicate finds its source");
        let gone = run(
            "DELETE FROM _dt_floatkey_probe WHERE \"k\" = ?::FLOAT",
            Some(vec![key.clone()]),
        );
        assert_eq!(gone.rows[0][0].as_u64(), Some(1), "{key}: one row deleted");
    }
    run("COMMIT", None);

    // A FLOAT value needs no cast in the SET or VALUES list: assignment
    // rounds the DOUBLE to the FLOAT the cast would have made.
    let stored = run("SELECT k, f, f = 2.2::FLOAT FROM _dt_floatkey_probe ORDER BY k", None);
    println!("the copies, keyed and set through a bare `?`: {:?}", stored.rows);
    assert_eq!(
        stored.rows,
        vec![
            vec![json!(10.1), json!(2.2), json!(true)],
            vec![json!(10.5), json!(2.2), json!(true)],
            vec![json!(11.1), json!(2.2), json!(true)],
        ]
    );
    run("DROP TABLE _dt_floatkey_probe", None);
}

/// Why DuckTable's `parse_value` refuses typed text for a container that
/// holds a VARIANT, a JSON or a BLOB. Such a cell is bound as the container's
/// displayed text through a bare `?`, and the engine's cast of that text
/// never reaches the inner value: the statement succeeds, and what is stored
/// is not what was shown.
#[test]
#[ignore]
fn a_container_of_documents_or_blobs_is_corrupted_by_its_own_text() {
    use serde_json::json;
    let Some(row) = connectable() else {
        println!("no berth to test against; skipping");
        return;
    };
    let conn = connect(&row.name).expect("connect");
    let sid = harbor_client::session_new(&conn).expect("session");
    let run = |sql: &str, params: Option<Vec<serde_json::Value>>| {
        harbor_client::exec(&conn, sql, params, Some(&sid)).expect(sql)
    };
    run("DROP TABLE IF EXISTS _dt_container_probe", None);
    run(
        "CREATE TEMP TABLE _dt_container_probe(id INTEGER PRIMARY KEY, bl BLOB[], fixed BLOB[2], \
         docs VARIANT[], js JSON[], s STRUCT(v VARIANT, n INTEGER), m MAP(VARCHAR, BLOB))",
        None,
    );
    run(
        "INSERT INTO _dt_container_probe VALUES (1, ['hi'::BLOB], ['hi'::BLOB, '\\xFF'::BLOB], \
         [{'a': 1}::VARIANT], ['{\"a\":1}'::JSON], {'v': {'a': 1}::VARIANT, 'n': 1}, MAP {'k': 'hi'::BLOB})",
        None,
    );
    let shown = run("SELECT bl, fixed, docs, js, s, m FROM _dt_container_probe", None);
    let columns: Vec<&str> = shown.columns.iter().map(|c| c.duckdb_type.as_str()).collect();
    println!("types: {columns:?}");
    assert_eq!(
        columns,
        ["BLOB[]", "BLOB[2]", "VARIANT[]", "JSON[]", "STRUCT(v VARIANT, n INTEGER)", "MAP(VARCHAR, BLOB)"]
    );

    // The text the grid shows for each cell, bound back the way a typed edit
    // of a nested type is: bare, for the engine to cast. Each column is
    // probed in its own transaction and rolled back.
    let text = |v: &serde_json::Value| match v {
        serde_json::Value::String(s) => json!(s),
        other => json!(other.to_string()),
    };
    // A MAP's pairs are not text the engine casts to a MAP, so its cell is
    // retyped the way the engine writes one, with the same base64.
    for (ix, (col, inner, typed)) in [
        ("bl", "bl[1]::VARCHAR", None),
        ("fixed", "fixed[1]::VARCHAR", None),
        ("docs", "variant_typeof(docs[1])", None),
        ("js", "json_type(js[1])", None),
        ("s", "variant_typeof(s.v)", None),
        ("m", "m['k']::VARCHAR", Some("{k=aGk=}")),
    ]
    .into_iter()
    .enumerate()
    {
        let read = format!("SELECT {inner} FROM _dt_container_probe");
        let before = run(&read, None).rows.remove(0).remove(0);
        let bound = typed.map(|t| json!(t)).unwrap_or_else(|| text(&shown.rows[0][ix]));
        run("BEGIN", None);
        let sql = format!("UPDATE _dt_container_probe SET \"{col}\" = ? WHERE \"id\" = ?");
        let hit = harbor_client::exec(&conn, &sql, Some(vec![bound.clone(), json!(1)]), Some(&sid));
        let hit = hit.unwrap_or_else(|e| panic!("{col} = {bound}: {e}"));
        assert_eq!(hit.rows[0][0].as_u64(), Some(1), "{col}: the statement succeeds");
        let after = run(&read, None).rows.remove(0).remove(0);
        println!("{col} = {bound}: {inner} was {before}, is {after}");
        assert_ne!(before, after, "{col}: the same text is not the same value");
        run("ROLLBACK", None);
    }
    run("DROP TABLE _dt_container_probe", None);
}

/// The JSON a document cell takes (ducktable's `check_json`): strict, and at
/// most 100 levels deep. A 100-level document binds through `?::JSON` into a
/// VARIANT and a JSON column and reads back whole. The engine's JSON also
/// reads NaN, which JSON does not have; the cell that comes back is then not
/// JSON, which is why the editor refuses it.
#[test]
#[ignore]
fn a_document_cell_takes_strict_json_a_hundred_levels_deep() {
    use serde_json::json;
    let Some(row) = connectable() else {
        println!("no berth to test against; skipping");
        return;
    };
    let conn = connect(&row.name).expect("connect");
    let sid = harbor_client::session_new(&conn).expect("session");
    let run = |sql: &str, params: Option<Vec<serde_json::Value>>| {
        harbor_client::exec(&conn, sql, params, Some(&sid)).expect(sql)
    };
    run("DROP TABLE IF EXISTS _dt_depth_probe", None);
    run("CREATE TEMP TABLE _dt_depth_probe(id INTEGER PRIMARY KEY, doc VARIANT, j JSON)", None);
    let deep = format!("{}1{}", "[{\"a\":".repeat(50), "}]".repeat(50));
    run("BEGIN", None);
    let made = run(
        "INSERT INTO _dt_depth_probe (\"id\", \"doc\", \"j\") VALUES (?, ?::JSON, ?::JSON) RETURNING id",
        Some(vec![json!(1), json!(deep), json!(deep)]),
    );
    assert_eq!(made.rows.len(), 1);
    let hit = run(
        "UPDATE _dt_depth_probe SET \"doc\" = ?::JSON, \"j\" = ?::JSON WHERE \"id\" = ?",
        Some(vec![json!(deep), json!(deep), json!(1)]),
    );
    assert_eq!(hit.rows[0][0].as_u64(), Some(1));
    run("COMMIT", None);
    let back = run("SELECT doc, j, variant_typeof(doc) FROM _dt_depth_probe", None);
    // A document cell reaches this client as its JSON text.
    let document = |cell: &serde_json::Value| -> Result<serde_json::Value, serde_json::Error> {
        serde_json::from_str(cell.as_str().expect("JSON text"))
    };
    let parsed: serde_json::Value = serde_json::from_str(&deep).unwrap();
    println!("100 levels: variant_typeof {}, {} bytes back", back.rows[0][2], back.rows[0][0].as_str().unwrap().len());
    assert_eq!(back.rows[0][2], json!("ARRAY(1)"));
    assert_eq!(document(&back.rows[0][0]).unwrap(), parsed, "the VARIANT reads back whole");
    assert_eq!(document(&back.rows[0][1]).unwrap(), parsed, "and so does the JSON column");

    // The same text into both: a JSON column keeps it, character for
    // character; a VARIANT keeps its values.
    let typed = "{ \"p\": 100.00,  \"a\": 1, \"a\": 2 }";
    let kept = run(
        "INSERT INTO _dt_depth_probe (\"id\", \"doc\", \"j\") VALUES (?, ?::JSON, ?::JSON) \
         RETURNING doc, j, variant_typeof(doc.p)",
        Some(vec![json!(3), json!(typed), json!(typed)]),
    );
    println!(
        "typed {typed:?}: the VARIANT reads back {} with p a {}, the JSON column {}",
        kept.rows[0][0], kept.rows[0][2], kept.rows[0][1]
    );
    assert_eq!(kept.rows[0][1], json!(typed), "a JSON column stores the text as typed");
    assert_eq!(kept.rows[0][2], json!("DOUBLE"), "and a JSON decimal is a DOUBLE in a VARIANT");
    assert_eq!(kept.rows[0][0], json!("{\"p\":100.0,\"a\":2}"), "a VARIANT reads back compact");

    let nan = run(
        "INSERT INTO _dt_depth_probe (\"id\", \"doc\") VALUES (?, ?::JSON) RETURNING doc, variant_typeof(doc.x)",
        Some(vec![json!(2), json!("{\"x\": NaN}")]),
    );
    println!("NaN through ?::JSON: {:?}", nan.rows[0]);
    assert_eq!(nan.rows[0][1], json!("DOUBLE"), "the engine takes NaN in a document");
    assert!(document(&nan.rows[0][0]).is_err(), "and what comes back is not JSON");
    run("DROP TABLE _dt_depth_probe", None);
}

/// Why DuckTable's `parse_value` holds a FLOAT cell to a FLOAT's range. A
/// typed number is bound as a JSON number, a DOUBLE, and assigned to the
/// column: the engine rounds it to the nearest FLOAT, and refuses one that
/// rounds past the largest — at commit, where the whole transaction goes
/// with it. A DOUBLE column takes the same numbers.
#[test]
#[ignore]
fn a_float_cell_holds_what_rounds_to_a_float() {
    use serde_json::json;
    let Some(row) = connectable() else {
        println!("no berth to test against; skipping");
        return;
    };
    println!("berth: {}", row.name);
    let conn = connect(&row.name).expect("connect");
    let sid = harbor_client::session_new(&conn).expect("session");
    let run = |sql: &str, params: Option<Vec<serde_json::Value>>| {
        harbor_client::exec(&conn, sql, params, Some(&sid)).expect(sql)
    };
    run("DROP TABLE IF EXISTS _dt_floatrange_probe", None);
    run("CREATE TEMP TABLE _dt_floatrange_probe(k INTEGER, f FLOAT, d DOUBLE)", None);
    let typed = run("SELECT f, d FROM _dt_floatrange_probe", None);
    println!("column types {:?}", typed.columns.iter().map(|c| &c.duckdb_type).collect::<Vec<_>>());
    assert_eq!(typed.columns[0].duckdb_type, "FLOAT");

    // The largest FLOAT, and a DOUBLE past it that still rounds to it.
    for text in ["3.4028235e38", "-3.4028235e38", "3.40282356e38", "1e-50"] {
        let v: f64 = text.parse().unwrap();
        let kept = run(
            "INSERT INTO _dt_floatrange_probe (k, f) VALUES (?, ?) RETURNING f",
            Some(vec![json!(1), json!(v)]),
        );
        println!("{text} into FLOAT: {}", kept.rows[0][0]);
        // The wire names the FLOAT by its shortest decimal.
        assert_eq!(kept.rows[0][0].as_f64().map(|k| k as f32), Some(v as f32), "{text}");
        assert!((v as f32).is_finite(), "{text}: the editor's test agrees");
    }
    // Halfway to the next power of two, and everything beyond it.
    for text in ["3.4028235677973366e38", "3.4028236e38", "3.5e38", "-3.5e38", "1e39"] {
        let v: f64 = text.parse().unwrap();
        let refused = harbor_client::exec(
            &conn,
            "INSERT INTO _dt_floatrange_probe (k, f) VALUES (?, ?)",
            Some(vec![json!(2), json!(v)]),
            Some(&sid),
        );
        println!("{text} into FLOAT: {:?}", refused.as_ref().err());
        assert!(refused.is_err(), "{text} is past a FLOAT");
        assert!(!(v as f32).is_finite(), "{text}: the editor's test agrees");
        let kept = run(
            "INSERT INTO _dt_floatrange_probe (k, d) VALUES (?, ?) RETURNING d",
            Some(vec![json!(3), json!(v)]),
        );
        assert_eq!(kept.rows[0][0].as_f64(), Some(v), "{text} is a DOUBLE");
    }
    run("DROP TABLE _dt_floatrange_probe", None);
}

/// Why text typed into a BLOB cell is never SQL NULL (ducktable's
/// `parse_value`). `null`, in any case, is four base64 characters, and
/// through the BLOB placeholder each spelling is three bytes a cell can hold
/// and show. NULL reaches the column as a NULL param, which is what ⌃⇧N and
/// Delete bind.
#[test]
#[ignore]
fn null_typed_into_a_blob_cell_is_three_bytes() {
    use serde_json::json;
    let Some(row) = connectable() else {
        println!("no berth to test against; skipping");
        return;
    };
    println!("berth: {}", row.name);
    let conn = connect(&row.name).expect("connect");
    let sid = harbor_client::session_new(&conn).expect("session");
    let run = |sql: &str, params: Option<Vec<serde_json::Value>>| {
        harbor_client::exec(&conn, sql, params, Some(&sid)).expect(sql)
    };
    run("DROP TABLE IF EXISTS _dt_blobnull_probe", None);
    run("CREATE TEMP TABLE _dt_blobnull_probe(k INTEGER, b BLOB)", None);
    for (text, bytes) in [("null", "9EE965"), ("NULL", "3542CB"), ("Null", "36E965")] {
        let kept = run(
            "INSERT INTO _dt_blobnull_probe (k, b) VALUES (?, from_base64(?::VARCHAR)) \
             RETURNING hex(b), b, b IS NULL",
            Some(vec![json!(1), json!(text)]),
        );
        println!("{text:?} through from_base64(?::VARCHAR): {:?}", kept.rows[0]);
        assert_eq!(kept.rows[0], vec![json!(bytes), json!(text), json!(false)]);
    }
    let null = run(
        "INSERT INTO _dt_blobnull_probe (k, b) VALUES (?, from_base64(?::VARCHAR)) RETURNING b IS NULL",
        Some(vec![json!(2), serde_json::Value::Null]),
    );
    println!("a NULL param through it: b IS NULL = {}", null.rows[0][0]);
    assert_eq!(null.rows[0][0], json!(true));
    run("DROP TABLE _dt_blobnull_probe", None);
}

/// Which types keep the whitespace around their text (ducktable's
/// `edits.rs`, `keeps_whitespace`). Padded text bound into a number, a date,
/// a list or a VARIANT is the value the bare text names, and a UUID, a BIT
/// and base64 refuse it; so padding alone is never a change to such a cell.
/// A VARCHAR, a JSON column and an ENUM store it: there it is another value.
#[test]
#[ignore]
fn padding_is_part_of_a_value_only_for_text_json_and_enum() {
    use serde_json::json;
    let Some(row) = connectable() else {
        println!("no berth to test against; skipping");
        return;
    };
    println!("berth: {}", row.name);
    let conn = connect(&row.name).expect("connect");
    let sid = harbor_client::session_new(&conn).expect("session");
    let run = |sql: &str, params: Option<Vec<serde_json::Value>>| {
        harbor_client::exec(&conn, sql, params, Some(&sid)).expect(sql)
    };
    run("DROP TABLE IF EXISTS _dt_padding_probe", None);
    for (ty, bare, placeholder) in [
        ("INTEGER", "5", "?"),
        ("HUGEINT", "170141183460469231731687303715884105727", "?"),
        ("DOUBLE", "1.5", "?"),
        ("DECIMAL(10,2)", "1.50", "?"),
        ("DATE", "2024-02-29", "?"),
        ("TIMESTAMP", "2024-02-29 01:02:03", "?"),
        ("TIME", "01:02:03", "?"),
        ("INTERVAL", "3 days", "?"),
        ("INTEGER[]", "[1, 2]", "?"),
        ("STRUCT(a INTEGER)", "{'a': 1}", "?"),
        ("VARIANT", "{\"a\":1}", "?::JSON"),
    ] {
        run(&format!("CREATE OR REPLACE TEMP TABLE _dt_padding_probe(k INTEGER, v {ty})"), None);
        let insert = format!("INSERT INTO _dt_padding_probe VALUES (?, {placeholder})");
        run(&insert, Some(vec![json!(1), json!(bare)]));
        run(&insert, Some(vec![json!(2), json!(format!(" {bare} "))]));
        let same = run("SELECT count(DISTINCT v::VARCHAR), min(v::VARCHAR) FROM _dt_padding_probe", None);
        println!("{ty}: padded and bare are {} value(s), {}", same.rows[0][0], same.rows[0][1]);
        assert_eq!(same.rows[0][0].as_u64(), Some(1), "{ty}");
    }
    for (ty, bare, placeholder) in [
        ("UUID", "6f9619ff-8b86-d011-b42d-00c04fc964ff", "?"),
        ("BIT", "101", "?"),
        ("BLOB", "qg==", "from_base64(?::VARCHAR)"),
    ] {
        run(&format!("CREATE OR REPLACE TEMP TABLE _dt_padding_probe(k INTEGER, v {ty})"), None);
        let insert = format!("INSERT INTO _dt_padding_probe VALUES (?, {placeholder})");
        run(&insert, Some(vec![json!(1), json!(bare)]));
        let refused =
            harbor_client::exec(&conn, &insert, Some(vec![json!(2), json!(format!(" {bare} "))]), Some(&sid));
        println!("{ty}: padded text is refused: {:?}", refused.as_ref().err().map(|e| e.lines().next().unwrap_or("").to_string()));
        assert!(refused.is_err(), "{ty}");
    }
    for (ty, bare, placeholder) in [
        ("VARCHAR", "5", "?"),
        ("JSON", "{\"a\":1}", "?::JSON"),
        ("ENUM('a', ' a ')", "a", "?"),
    ] {
        run(&format!("CREATE OR REPLACE TEMP TABLE _dt_padding_probe(k INTEGER, v {ty})"), None);
        let insert = format!("INSERT INTO _dt_padding_probe VALUES (?, {placeholder})");
        run(&insert, Some(vec![json!(1), json!(bare)]));
        run(&insert, Some(vec![json!(2), json!(format!(" {bare} "))]));
        let kept = run("SELECT v::VARCHAR FROM _dt_padding_probe ORDER BY k", None);
        println!("{ty}: stored {:?}", kept.rows);
        assert_eq!(kept.rows, vec![vec![json!(bare)], vec![json!(format!(" {bare} "))]], "{ty}");
    }
    run("DROP TABLE _dt_padding_probe", None);
}

/// How a grid notices a table altered elsewhere (ducktable's `grid.rs`,
/// `same_columns`). One client pages a table; another alters a column's type
/// and adds a column; the first client's next page of the same SQL carries
/// the new names and types, which is all the signal there is. The stale type
/// matters: base64 text bound through the placeholder of the VARCHAR the
/// column was is stored in the BLOB it became as the characters themselves.
#[test]
#[ignore]
fn a_table_altered_elsewhere_shows_in_the_next_page() {
    use serde_json::json;
    let Some(row) = connectable() else {
        println!("no berth to test against; skipping");
        return;
    };
    println!("berth: {}", row.name);
    let grid = connect(&row.name).expect("connect");
    let other = connect(&row.name).expect("connect again");
    let elsewhere = harbor_client::session_new(&other).expect("session");
    let alter = |sql: &str| harbor_client::exec(&other, sql, None, Some(&elsewhere)).expect(sql);
    let shape = |r: &harbor_client::QueryResult| -> Vec<(String, String)> {
        r.columns.iter().map(|c| (c.name.clone().unwrap_or_default(), c.duckdb_type.clone())).collect()
    };
    alter("DROP TABLE IF EXISTS main._dt_reshape_probe");
    alter("CREATE TABLE main._dt_reshape_probe(id INTEGER PRIMARY KEY, payload VARCHAR)");
    alter("INSERT INTO main._dt_reshape_probe VALUES (1, 'qg==')");

    let page = "SELECT * FROM \"main\".\"_dt_reshape_probe\" LIMIT 500 OFFSET 0";
    let born = harbor_client::query(&grid, page).expect("first page");
    println!("the grid's birth: {:?}", shape(&born));
    assert_eq!(shape(&born), vec![("id".into(), "INTEGER".into()), ("payload".into(), "VARCHAR".into())]);

    alter("ALTER TABLE main._dt_reshape_probe ALTER payload TYPE BLOB");
    alter("ALTER TABLE main._dt_reshape_probe ADD COLUMN note VARCHAR DEFAULT 'n'");
    let next = harbor_client::query(&grid, page).expect("next page");
    println!("the next page:    {:?}", shape(&next));
    assert_eq!(
        shape(&next),
        vec![("id".into(), "INTEGER".into()), ("payload".into(), "BLOB".into()), ("note".into(), "VARCHAR".into())]
    );
    assert_ne!(shape(&born), shape(&next));

    // What the stale VARCHAR placeholder does to the BLOB, and what the
    // BLOB's own placeholder does.
    for (id, placeholder) in [(2, "?"), (3, "from_base64(?::VARCHAR)")] {
        let sql = format!("INSERT INTO main._dt_reshape_probe (id, payload) VALUES (?, {placeholder}) RETURNING hex(payload)");
        let kept = harbor_client::exec(&grid, &sql, Some(vec![json!(id), json!("qrs=")]), None).expect("insert");
        println!("'qrs=' through `{placeholder}`: bytes {}", kept.rows[0][0]);
    }
    let stored = harbor_client::query(&grid, "SELECT hex(payload) FROM main._dt_reshape_probe WHERE id > 1 ORDER BY id")
        .expect("read back");
    assert_eq!(stored.rows, vec![vec![json!("7172733D")], vec![json!("AABB")]]);
    alter("DROP TABLE main._dt_reshape_probe");
    harbor_client::session_release(&other, &elsewhere);
}
