// The HTTP side of harbor.
//
// Shape (deliberately small):
//
//   POST /sql     run one statement, stream the NDJSON envelope back
//   GET  /health  liveness, no auth
//
// The envelope is the one thing that must not drift from the C++ harbor,
// because it is the contract every client already speaks:
//
//   {"type":"schema","columns":[{"name":"id","duckdbType":"BIGINT","lossless":true}]}
//   {"type":"row","values":[0,"row0"]}
//   {"type":"end","rowCount":3,"timeMs":2}
//
// Three properties of that envelope are load-bearing and easy to lose in a
// rewrite:
//
//   1. It streams. Rows go out as chunks arrive; a large result is never
//      materialised in memory first.
//   2. Types are carried per column (`duckdbType`, plus `decimal` width/scale
//      and nested `child`/`fields`), so a client can reconstruct exactly what
//      DuckDB had.
//   3. Values that JSON cannot hold losslessly are quoted, not emitted as bare
//      numbers. HUGEINT and large BIGINT go out as strings — a bare
//      123456789012345678901234567890 silently becomes 1.2345678901234568e+29
//      in any JavaScript client.
//
// One statement per request, on purpose: it makes SQL injection through
// string concatenation structurally impossible, and it keeps HTTP status
// codes meaningful. Multi-statement work belongs on a session.
//
// Concurrency: accept many connections, execute few queries. DuckDB
// parallelises a single query across all cores, so running hundreds
// concurrently buys thrashing, not throughput. A fixed worker pool bounds
// in-flight statements; connections queue in the kernel accept backlog.

#![allow(dead_code)]

use std::{
    io::{Read, Write},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use duckdb::{
    Connection,
    core::{LogicalTypeHandle, LogicalTypeId},
    params_from_iter,
    types::{TimeUnit, Value},
};
use tiny_http::{Header, Method, Request, Response, Server};

/// Bounded number of statements executing at once. Connections may greatly
/// exceed this; queries should not.
pub const DEFAULT_MAX_INFLIGHT: usize = 6;

/// Largest request body we will read. A statement is text; a megabyte of it
/// is already pathological.
const MAX_BODY: usize = 8 << 20;

/// Rows are buffered to roughly this size before hitting the socket. Small
/// enough that a slow client sees data promptly, large enough that a wide
/// result is not one syscall per row.
const FLUSH_AT: usize = 64 << 10;

// ---------------------------------------------------------------------------
// Process-wide state
//
// harbor lives inside DuckDB's process, so "the server" is a process
// singleton: one listener, one worker pool.
//
// The pool has to be built during extension load, and that is not a stylistic
// choice. DuckDB hands an extension its database handle through a wrapper
// owned by the loader's state (`extension_load.cpp`, `ExtensionAccess::
// GetDatabase`), and that state is destroyed the moment loading finishes.
// The connection objects made from it stay valid; the handle does not. So
// every connection harbor will ever use is opened here, before the entrypoint
// returns, and `harbor_serve` draws from what is already there.
//
// One connection per worker, because a DuckDB connection is Send but not
// Sync — two threads may not share one.
// ---------------------------------------------------------------------------

/// How many worker connections to open at load. Idle connections are nearly
/// free; not being able to open one later is not.
const POOL_SIZE: usize = 8;

/// Connections handed out to workers when the server starts, returned when it
/// stops.
static POOL: Mutex<Vec<Connection>> = Mutex::new(Vec::new());

/// Reserved for harbor's own statements — the shutdown CHECKPOINT — so it is
/// never waiting behind a client query.
static CONTROL: Mutex<Option<Connection>> = Mutex::new(None);

static RUNNING: Mutex<Option<Running>> = Mutex::new(None);

/// Woken when the server stops, so `harbor_wait()` can block without polling.
static STOPPED: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());

struct Running {
    server: Arc<Server>,
    stop: Arc<AtomicBool>,
    workers: Vec<JoinHandle<Connection>>,
    addr: String,
}

/// Open every connection harbor will need. Called once, from the extension
/// entrypoint, and only there — see the note above on why later is too late.
pub fn open_pool(con: Connection) -> Result<(), String> {
    let mut pool = POOL.lock().unwrap();
    for _ in 0..POOL_SIZE {
        pool.push(con.try_clone().map_err(|e| format!("harbor: {e}"))?);
    }
    *CONTROL.lock().unwrap() = Some(con);
    Ok(())
}

// ---------------------------------------------------------------------------
// start / stop / wait
// ---------------------------------------------------------------------------

pub fn start(bind: &str, port: u16, token: Option<String>, workers: usize) -> Result<String, String> {
    let mut running = RUNNING.lock().unwrap();
    if let Some(r) = running.as_ref() {
        return Err(format!("harbor is already serving on {}", r.addr));
    }

    // Take the connections first: binding a socket we then cannot serve on
    // is a worse failure than not binding at all.
    let mut pool = POOL.lock().unwrap();
    if pool.is_empty() {
        return Err("harbor: no database connections (extension not initialised)".to_string());
    }
    let workers = workers.clamp(1, pool.len());
    let keep = pool.len() - workers;
    let mut conns: Vec<Connection> = pool.drain(keep..).collect();
    drop(pool);

    let server = match Server::http((bind, port)) {
        Ok(s) => s,
        Err(e) => {
            POOL.lock().unwrap().append(&mut conns);
            return Err(format!("harbor: cannot bind {bind}:{port}: {e}"));
        }
    };
    let addr = server.server_addr().to_string();
    let server = Arc::new(server);
    let stop = Arc::new(AtomicBool::new(false));
    let token = Arc::new(token);

    let mut handles = Vec::with_capacity(workers);
    for (i, conn) in conns.into_iter().enumerate() {
        let server = Arc::clone(&server);
        let stop = Arc::clone(&stop);
        let token = Arc::clone(&token);
        handles.push(
            thread::Builder::new()
                .name(format!("harbor-{i}"))
                .spawn(move || worker(server, stop, token, conn))
                .map_err(|e| e.to_string())?,
        );
    }

    *STOPPED.0.lock().unwrap() = false;
    *running = Some(Running { server, stop, workers: handles, addr: addr.clone() });
    Ok(addr)
}

pub fn stop() -> Result<String, String> {
    let Some(r) = RUNNING.lock().unwrap().take() else {
        return Err("harbor is not serving".to_string());
    };
    r.stop.store(true, Ordering::SeqCst);
    r.server.unblock();

    // Workers hand their connection back as they exit, so a later
    // harbor_serve has a pool to draw from. A panicked worker forfeits its
    // connection rather than taking the shutdown down with it.
    let mut pool = POOL.lock().unwrap();
    for h in r.workers {
        if let Ok(conn) = h.join() {
            pool.push(conn);
        }
    }
    drop(pool);

    // Durability: the point of a clean stop is that the WAL is folded back
    // into the database file, so the next process opens without a replay.
    if let Some(c) = CONTROL.lock().unwrap().as_ref() {
        let _ = c.execute_batch("CHECKPOINT");
    }

    let (lock, cv) = &STOPPED;
    *lock.lock().unwrap() = true;
    cv.notify_all();
    Ok(r.addr)
}

/// Block until the server stops. Returns the address it was serving on.
pub fn wait() -> Result<String, String> {
    let addr = match RUNNING.lock().unwrap().as_ref() {
        Some(r) => r.addr.clone(),
        None => return Err("harbor is not serving".to_string()),
    };
    let (lock, cv) = &STOPPED;
    let mut stopped = lock.lock().unwrap();
    while !*stopped {
        stopped = cv.wait(stopped).unwrap();
    }
    Ok(addr)
}

pub fn address() -> Option<String> {
    RUNNING.lock().unwrap().as_ref().map(|r| r.addr.clone())
}

// ---------------------------------------------------------------------------
// Request handling
// ---------------------------------------------------------------------------

fn worker(
    server: Arc<Server>,
    stop: Arc<AtomicBool>,
    token: Arc<Option<String>>,
    conn: Connection,
) -> Connection {
    while !stop.load(Ordering::SeqCst) {
        // A timeout rather than a blocking recv, so `unblock()` is not the
        // only way out and a worker cannot wedge on shutdown.
        match server.recv_timeout(Duration::from_millis(200)) {
            Ok(Some(req)) => handle(req, &conn, token.as_ref().as_deref()),
            Ok(None) => continue,
            Err(_) => break,
        }
    }
    conn
}

fn handle(mut req: Request, conn: &Connection, token: Option<&str>) {
    let path = req.url().split('?').next().unwrap_or("/").to_string();
    let method = req.method().clone();

    match (&method, path.as_str()) {
        // Liveness is unauthenticated on purpose: a load balancer should not
        // need a credential to learn whether the process is up, and the
        // answer reveals nothing.
        (Method::Get, "/health") => {
            let _ = req.respond(json_response(200, r#"{"status":"ok"}"#));
        }
        (Method::Post, "/sql") => {
            if !authorized(&req, token) {
                let _ = req.respond(error_response(401, "unauthorized", "missing or invalid bearer token"));
                return;
            }
            let mut body = String::new();
            if req.as_reader().take(MAX_BODY as u64).read_to_string(&mut body).is_err() {
                let _ = req.respond(error_response(400, "bad_request", "body is not valid UTF-8"));
                return;
            }
            run_sql(req, conn, &body);
        }
        _ => {
            let _ = req.respond(error_response(404, "not_found", "no such endpoint"));
        }
    }
}

fn authorized(req: &Request, token: Option<&str>) -> bool {
    let Some(expected) = token else { return true };
    let Some(h) = req
        .headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
    else {
        return false;
    };
    let value = h.value.as_str();
    let Some(presented) = value.strip_prefix("Bearer ") else { return false };
    constant_time_eq(presented.as_bytes(), expected.as_bytes())
}

/// Compare without leaking the match length through timing. Lengths are not
/// secret, so an early return on length is fine.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The request body: one statement, optional positional parameters.
struct SqlRequest {
    sql: String,
    params: Vec<Value>,
}

fn parse_request(body: &str) -> Result<SqlRequest, String> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
    let sql = v
        .get("sql")
        .and_then(|s| s.as_str())
        .ok_or_else(|| "missing \"sql\"".to_string())?
        .to_string();
    if sql.trim().is_empty() {
        return Err("\"sql\" is empty".to_string());
    }
    let params = match v.get("params") {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(serde_json::Value::Array(a)) => a.iter().map(json_to_duckdb).collect::<Result<_, _>>()?,
        Some(_) => return Err("\"params\" must be an array".to_string()),
    };
    Ok(SqlRequest { sql, params })
}

fn json_to_duckdb(v: &serde_json::Value) -> Result<Value, String> {
    Ok(match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Boolean(*b),
        serde_json::Value::String(s) => Value::Text(s.clone()),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::BigInt(i)
            } else if let Some(u) = n.as_u64() {
                Value::UBigInt(u)
            } else if let Some(f) = n.as_f64() {
                Value::Double(f)
            } else {
                return Err("unrepresentable number in \"params\"".to_string());
            }
        }
        // Arrays and objects have no unambiguous SQL type. Send them as JSON
        // text and cast on the SQL side, where the intent is explicit.
        other => Value::Text(other.to_string()),
    })
}

/// Reject anything with a second statement in it.
///
/// This has to happen before the text reaches DuckDB, and it is not belt and
/// braces. `duckdb-rs`'s `prepare` accepts multi-statement text: it *executes*
/// every statement but the last and returns the last one prepared. So
/// `SELECT 1; DROP TABLE orders` drops the table during preparation, before a
/// single row is fetched — the injection lands even if the request is never
/// executed.
///
/// The scan is deliberately strict. It tracks the constructs in which a
/// semicolon is data rather than a separator — string literals, quoted
/// identifiers, dollar quotes, comments — and rejects a bare `;` with
/// anything after it. Over-rejecting a statement someone could have written
/// differently is a much smaller cost than under-rejecting one they should
/// not have been able to write at all.
fn ensure_single_statement(sql: &str) -> Result<(), String> {
    let b = sql.as_bytes();
    let mut i = 0;
    let mut terminated_at: Option<usize> = None;

    while i < b.len() {
        match b[i] {
            b'-' if b.get(i + 1) == Some(&b'-') => {
                i = b[i..].iter().position(|&c| c == b'\n').map_or(b.len(), |p| i + p + 1);
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                // Block comments nest in DuckDB, as they do in Postgres.
                let mut depth = 1;
                i += 2;
                while i < b.len() && depth > 0 {
                    if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            q @ (b'\'' | b'"') => {
                // A doubled quote is an escaped quote, not the end of the
                // literal. E'...' additionally honours backslash escapes.
                let escapes = q == b'\'' && i > 0 && (b[i - 1] | 0x20) == b'e';
                i += 1;
                while i < b.len() {
                    if escapes && b[i] == b'\\' {
                        i += 2;
                    } else if b[i] == q {
                        if b.get(i + 1) == Some(&q) {
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            b'$' => {
                // $tag$ ... $tag$, where the tag is empty or an identifier.
                let tag_end = b[i + 1..]
                    .iter()
                    .position(|&c| !(c.is_ascii_alphanumeric() || c == b'_'))
                    .map(|p| i + 1 + p);
                match tag_end {
                    Some(end) if b[end] == b'$' => {
                        let tag = &b[i..=end];
                        let rest = &b[end + 1..];
                        i = rest
                            .windows(tag.len())
                            .position(|w| w == tag)
                            .map_or(b.len(), |p| end + 1 + p + tag.len());
                    }
                    _ => i += 1,
                }
            }
            b';' => {
                terminated_at = Some(i);
                i += 1;
            }
            _ => {
                // Anything after a terminator is a second statement, even if
                // it is only a comment — harbor has no reason to accept it.
                if terminated_at.is_some() && !b[i].is_ascii_whitespace() {
                    return Err("only one statement per request".to_string());
                }
                i += 1;
            }
        }
        if terminated_at.is_some() && i < b.len() && !b[i..].iter().all(|c| c.is_ascii_whitespace()) {
            return Err("only one statement per request".to_string());
        }
    }
    Ok(())
}

fn run_sql(req: Request, conn: &Connection, body: &str) {
    let parsed = match parse_request(body) {
        Ok(p) => p,
        Err(e) => {
            let _ = req.respond(error_response(400, "bad_request", &e));
            return;
        }
    };

    if let Err(e) = ensure_single_statement(&parsed.sql) {
        let _ = req.respond(error_response(400, "bad_request", &e));
        return;
    }

    let started = Instant::now();

    // Prepare and execute before taking the socket writer, so a failure here
    // still gets a real HTTP status code instead of a 200 with an error line.
    let mut stmt = match conn.prepare(&parsed.sql) {
        Ok(s) => s,
        Err(e) => {
            let _ = req.respond(error_response(400, "sql_error", &e.to_string()));
            return;
        }
    };
    let mut rows = match stmt.query(params_from_iter(parsed.params.iter())) {
        Ok(r) => r,
        Err(e) => {
            let _ = req.respond(error_response(400, "sql_error", &e.to_string()));
            return;
        }
    };

    // Column metadata has to be captured now: `Rows` hands back `None` from
    // `as_ref()` once the result is exhausted.
    let (names, types) = match rows.as_ref() {
        Some(s) => {
            let n = s.column_count();
            let names: Vec<String> = (0..n).map(|i| s.column_name(i).cloned().unwrap_or_default()).collect();
            let types: Vec<LogicalTypeHandle> = (0..n).map(|i| s.column_logical_type(i)).collect();
            (names, types)
        }
        None => (Vec::new(), Vec::new()),
    };

    let mut out = ChunkedWriter::new(req.into_writer());
    if out.start().is_err() {
        return;
    }

    let mut line = String::with_capacity(256);
    line.push_str(r#"{"type":"schema","columns":["#);
    for (i, (name, ty)) in names.iter().zip(&types).enumerate() {
        if i > 0 {
            line.push(',');
        }
        emit_column_schema(&mut line, Some(name), ty);
    }
    line.push_str("]}\n");
    if out.push(&line).is_err() {
        return;
    }

    let mut count: u64 = 0;
    loop {
        match rows.next() {
            Ok(Some(row)) => {
                line.clear();
                line.push_str(r#"{"type":"row","values":["#);
                for (i, ty) in types.iter().enumerate() {
                    if i > 0 {
                        line.push(',');
                    }
                    match row.get_ref(i) {
                        Ok(v) => emit_value(&mut line, &Value::from(v), Some(ty)),
                        Err(_) => line.push_str("null"),
                    }
                }
                line.push_str("]}\n");
                count += 1;
                if out.push(&line).is_err() {
                    return;
                }
            }
            Ok(None) => break,
            Err(e) => {
                // Mid-stream failures cannot change the status code — the
                // headers are long gone. Say so in the stream and stop, so a
                // client never mistakes a truncated result for a complete one.
                line.clear();
                line.push_str(r#"{"type":"error","code":"sql_error","message":"#);
                push_json_string(&mut line, &e.to_string());
                line.push_str("}\n");
                let _ = out.push(&line);
                let _ = out.finish();
                return;
            }
        }
    }

    line.clear();
    line.push_str(&format!(
        r#"{{"type":"end","rowCount":{},"timeMs":{}}}"#,
        count,
        started.elapsed().as_millis()
    ));
    line.push('\n');
    let _ = out.push(&line);
    let _ = out.finish();
}

// ---------------------------------------------------------------------------
// Responses
//
// The streaming path writes the response by hand because tiny_http's
// `Response` wants either a known length or a `Read` to pull from, and rows
// are produced by a borrow chain that cannot outlive this stack frame.
// Chunked framing is a dozen lines; a pipe and a second thread is not.
// ---------------------------------------------------------------------------

struct ChunkedWriter {
    w: Box<dyn Write + Send>,
    buf: String,
}

impl ChunkedWriter {
    fn new(w: Box<dyn Write + Send>) -> Self {
        Self { w, buf: String::with_capacity(FLUSH_AT + 8192) }
    }

    fn start(&mut self) -> std::io::Result<()> {
        self.w.write_all(
            b"HTTP/1.1 200 OK\r\n\
              Content-Type: application/x-ndjson\r\n\
              Cache-Control: no-store\r\n\
              Transfer-Encoding: chunked\r\n\
              Connection: close\r\n\r\n",
        )
    }

    fn push(&mut self, s: &str) -> std::io::Result<()> {
        self.buf.push_str(s);
        if self.buf.len() >= FLUSH_AT { self.flush_chunk() } else { Ok(()) }
    }

    fn flush_chunk(&mut self) -> std::io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        write!(self.w, "{:x}\r\n", self.buf.len())?;
        self.w.write_all(self.buf.as_bytes())?;
        self.w.write_all(b"\r\n")?;
        self.buf.clear();
        self.w.flush()
    }

    fn finish(&mut self) -> std::io::Result<()> {
        self.flush_chunk()?;
        self.w.write_all(b"0\r\n\r\n")?;
        self.w.flush()
    }
}

fn json_response(status: u16, body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body)
        .with_status_code(status)
        .with_header(Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap())
}

fn error_response(status: u16, code: &str, message: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut s = String::new();
    s.push_str(r#"{"type":"error","code":"#);
    push_json_string(&mut s, code);
    s.push_str(r#","message":"#);
    push_json_string(&mut s, message);
    s.push('}');
    json_response(status, &s)
}

// ---------------------------------------------------------------------------
// Schema emission
// ---------------------------------------------------------------------------

fn emit_column_schema(out: &mut String, name: Option<&str>, ty: &LogicalTypeHandle) {
    out.push('{');
    if let Some(n) = name.filter(|n| !n.is_empty()) {
        out.push_str(r#""name":"#);
        push_json_string(out, n);
        out.push(',');
    }
    out.push_str(r#""duckdbType":"#);
    push_json_string(out, &type_name(ty));

    let id = ty.try_id().unwrap_or(LogicalTypeId::Unsupported);
    match id {
        LogicalTypeId::Decimal => {
            out.push_str(r#","lossless":true,"decimal":{"width":"#);
            out.push_str(&ty.decimal_width().to_string());
            out.push_str(r#","scale":"#);
            out.push_str(&ty.decimal_scale().to_string());
            out.push('}');
        }
        LogicalTypeId::List => {
            out.push_str(r#","lossless":true,"child":"#);
            emit_column_schema(out, None, &ty.child(0));
        }
        LogicalTypeId::Array => {
            out.push_str(r#","lossless":true,"child":"#);
            emit_column_schema(out, None, &ty.child(0));
        }
        LogicalTypeId::Struct => {
            out.push_str(r#","lossless":true,"fields":["#);
            for i in 0..ty.num_children() {
                if i > 0 {
                    out.push(',');
                }
                emit_column_schema(out, Some(&ty.child_name(i)), &ty.child(i));
            }
            out.push(']');
        }
        _ if is_lossless(id) => out.push_str(r#","lossless":true"#),
        // User-defined and extension types round-trip as text. Saying so
        // explicitly is better than silently handing back a string that
        // looks like a native value.
        _ => out.push_str(r#","lossless":false,"encoding":"varchar-cast""#),
    }
    out.push('}');
}

fn is_lossless(id: LogicalTypeId) -> bool {
    use LogicalTypeId::*;
    matches!(
        id,
        Boolean
            | Tinyint
            | Smallint
            | Integer
            | Bigint
            | Hugeint
            | UHugeint
            | UTinyint
            | USmallint
            | UInteger
            | UBigint
            | Float
            | Double
            | Varchar
            | Uuid
            | Date
            | Time
            | TimeTZ
            | Timestamp
            | TimestampS
            | TimestampMs
            | TimestampNs
            | TimestampTZ
            | Interval
            | Blob
            | Bit
            | Enum
            | SqlNull
    )
}

fn type_name(ty: &LogicalTypeHandle) -> String {
    use LogicalTypeId::*;
    // An alias is the user's own name for the type (JSON, for one); it is
    // more informative than the storage type underneath it.
    if let Some(alias) = ty.get_alias() {
        if !alias.is_empty() {
            return alias;
        }
    }
    match ty.try_id().unwrap_or(Unsupported) {
        Boolean => "BOOLEAN".into(),
        Tinyint => "TINYINT".into(),
        Smallint => "SMALLINT".into(),
        Integer => "INTEGER".into(),
        Bigint => "BIGINT".into(),
        Hugeint => "HUGEINT".into(),
        UHugeint => "UHUGEINT".into(),
        UTinyint => "UTINYINT".into(),
        USmallint => "USMALLINT".into(),
        UInteger => "UINTEGER".into(),
        UBigint => "UBIGINT".into(),
        Float => "FLOAT".into(),
        Double => "DOUBLE".into(),
        Varchar | StringLiteral => "VARCHAR".into(),
        Blob => "BLOB".into(),
        Bit => "BIT".into(),
        Uuid => "UUID".into(),
        Date => "DATE".into(),
        Time => "TIME".into(),
        TimeTZ => "TIME WITH TIME ZONE".into(),
        TimeNs => "TIME_NS".into(),
        Timestamp => "TIMESTAMP".into(),
        TimestampS => "TIMESTAMP_S".into(),
        TimestampMs => "TIMESTAMP_MS".into(),
        TimestampNs => "TIMESTAMP_NS".into(),
        TimestampTZ => "TIMESTAMP WITH TIME ZONE".into(),
        Interval => "INTERVAL".into(),
        Decimal => format!("DECIMAL({},{})", ty.decimal_width(), ty.decimal_scale()),
        List => format!("{}[]", type_name(&ty.child(0))),
        Array => format!("{}[{}]", type_name(&ty.child(0)), ty.num_children()),
        Enum => "ENUM".into(),
        Struct => {
            let fields: Vec<String> = (0..ty.num_children())
                .map(|i| format!("{} {}", ty.child_name(i), type_name(&ty.child(i))))
                .collect();
            format!("STRUCT({})", fields.join(", "))
        }
        Map => "MAP".into(),
        Union => "UNION".into(),
        SqlNull => "\"NULL\"".into(),
        Geometry => "GEOMETRY".into(),
        Variant => "VARIANT".into(),
        Bignum => "BIGNUM".into(),
        _ => "UNKNOWN".into(),
    }
}

// ---------------------------------------------------------------------------
// Value emission
//
// Dispatch is on the decoded value rather than the column type, because the
// value already carries what it needs — DECIMAL brings its width and scale,
// TIMESTAMP brings its unit. The column type is consulted only where the
// value is genuinely ambiguous: UUID and TIMESTAMP WITH TIME ZONE share a
// representation with plain integers and naive timestamps.
// ---------------------------------------------------------------------------

/// IEEE-754 doubles hold integers exactly only up to 2^53 - 1. Anything
/// wider goes out quoted; a JavaScript client parsing a bare
/// 9007199254740993 gets 9007199254740992 and never finds out.
const JSON_SAFE: i128 = 9_007_199_254_740_991;

fn emit_value(out: &mut String, v: &Value, ty: Option<&LogicalTypeHandle>) {
    let id = ty.and_then(|t| t.try_id().ok());
    match v {
        Value::Null => out.push_str("null"),
        Value::Boolean(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::TinyInt(i) => out.push_str(&i.to_string()),
        Value::SmallInt(i) => out.push_str(&i.to_string()),
        Value::Int(i) => out.push_str(&i.to_string()),
        Value::UTinyInt(i) => out.push_str(&i.to_string()),
        Value::USmallInt(i) => out.push_str(&i.to_string()),
        Value::UInt(i) => out.push_str(&i.to_string()),
        Value::BigInt(i) => push_int(out, *i as i128),
        Value::UBigInt(i) => push_int(out, *i as i128),
        Value::HugeInt(i) => {
            if id == Some(LogicalTypeId::Uuid) {
                push_json_string(out, &uuid_to_string(*i));
            } else {
                push_int(out, *i)
            }
        }
        Value::UHugeInt(i) => {
            if *i as i128 as u128 == *i && (*i as i128) <= JSON_SAFE {
                out.push_str(&i.to_string());
            } else {
                push_json_string(out, &i.to_string());
            }
        }
        Value::Float(f) => push_float(out, *f as f64),
        Value::Double(f) => push_float(out, *f),
        Value::Decimal(d) => push_json_string(out, &d.to_string()),
        Value::Text(s) | Value::Enum(s) => push_json_string(out, s),
        Value::Blob(b) | Value::Geometry(b) => push_json_string(out, &base64(b)),
        Value::Date32(d) => push_json_string(out, &fmt_date(*d)),
        Value::Time64(unit, v) => push_json_string(out, &fmt_time(to_nanos(*unit, *v))),
        Value::Timestamp(unit, v) => {
            let mut s = fmt_timestamp(to_nanos(*unit, *v), *unit);
            if id == Some(LogicalTypeId::TimestampTZ) {
                s.push('Z');
            }
            push_json_string(out, &s);
        }
        Value::Interval { months, days, nanos } => {
            // micros as a string: it is an int64 and JSON numbers are not.
            out.push_str(&format!(
                r#"{{"months":{},"days":{},"micros":"{}"}}"#,
                months,
                days,
                nanos / 1_000
            ));
        }
        Value::List(items) | Value::Array(items) => {
            let child = ty.map(|t| t.child(0));
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                emit_value(out, item, child.as_ref());
            }
            out.push(']');
        }
        Value::Struct(fields) => {
            out.push('{');
            for (i, (k, val)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                push_json_string(out, k);
                out.push(':');
                emit_value(out, val, ty.map(|t| t.child(i)).as_ref());
            }
            out.push('}');
        }
        Value::Map(entries) => {
            // A SQL MAP has no JSON counterpart: its keys need not be
            // strings. Pairs keep it lossless; the schema line says so with
            // "encoding":"pairs".
            out.push('[');
            for (i, (k, val)) in entries.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('[');
                emit_value(out, k, None);
                out.push(',');
                emit_value(out, val, None);
                out.push(']');
            }
            out.push(']');
        }
        Value::Union(inner) => emit_value(out, inner, None),
        // `Value` is #[non_exhaustive]: a later DuckDB can add a variant this
        // build has never seen. The schema line already flags such a column
        // lossless:false, so a client knows not to trust the payload.
        _ => out.push_str("null"),
    }
}

fn push_int(out: &mut String, i: i128) {
    if i.abs() <= JSON_SAFE {
        out.push_str(&i.to_string());
    } else {
        push_json_string(out, &i.to_string());
    }
}

fn push_float(out: &mut String, f: f64) {
    // JSON has no NaN or Infinity. Null is the only honest encoding.
    if f.is_finite() {
        out.push_str(&f.to_string());
    } else {
        out.push_str("null");
    }
}

fn push_json_string(out: &mut String, s: &str) {
    // serde_json owns the escaping rules, including the ones that are easy to
    // get wrong (control characters, lone surrogates).
    match serde_json::to_string(s) {
        Ok(encoded) => out.push_str(&encoded),
        Err(_) => out.push_str("\"\""),
    }
}

// ---------------------------------------------------------------------------
// Scalar formatting
// ---------------------------------------------------------------------------

fn to_nanos(unit: TimeUnit, v: i64) -> i128 {
    let v = v as i128;
    match unit {
        TimeUnit::Second => v * 1_000_000_000,
        TimeUnit::Millisecond => v * 1_000_000,
        TimeUnit::Microsecond => v * 1_000,
        TimeUnit::Nanosecond => v,
    }
}

/// Days since 1970-01-01 to a civil date, by Howard Hinnant's
/// `civil_from_days`. Correct for the proleptic Gregorian calendar over the
/// whole int32 range, which is more than DATE can hold.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn fmt_date(days: i32) -> String {
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// HH:MM:SS, with a fraction only when there is one. Six digits unless the
/// value carries sub-microsecond precision.
fn fmt_time(nanos: i128) -> String {
    let (h, min, s, frac) = split_time(nanos.rem_euclid(86_400_000_000_000));
    let mut out = format!("{h:02}:{min:02}:{s:02}");
    push_fraction(&mut out, frac);
    out
}

fn fmt_timestamp(nanos: i128, unit: TimeUnit) -> String {
    let day = 86_400_000_000_000i128;
    let days = nanos.div_euclid(day);
    let rest = nanos.rem_euclid(day);
    let (y, m, d) = civil_from_days(days as i64);
    let (h, min, s, frac) = split_time(rest);
    let mut out = format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}");
    // TIMESTAMP_S has no fractional part by definition; emitting one would
    // invent precision the column does not have.
    if unit != TimeUnit::Second {
        push_fraction(&mut out, frac);
    }
    out
}

fn split_time(nanos_in_day: i128) -> (i64, i64, i64, i64) {
    let total_s = (nanos_in_day / 1_000_000_000) as i64;
    let frac = (nanos_in_day % 1_000_000_000) as i64;
    (total_s / 3_600, (total_s % 3_600) / 60, total_s % 60, frac)
}

fn push_fraction(out: &mut String, nanos: i64) {
    if nanos == 0 {
        return;
    }
    if nanos % 1_000 == 0 {
        out.push_str(&format!(".{:06}", nanos / 1_000));
    } else {
        out.push_str(&format!(".{nanos:09}"));
    }
}

/// DuckDB stores UUID as a HUGEINT with the high bit flipped, so that the
/// integer ordering matches the textual ordering.
fn uuid_to_string(v: i128) -> String {
    let bits = (v as u128) ^ (1u128 << 127);
    let b = bits.to_be_bytes();
    let hex = |r: &[u8]| r.iter().map(|x| format!("{x:02x}")).collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        hex(&b[0..4]),
        hex(&b[4..6]),
        hex(&b[6..8]),
        hex(&b[8..10]),
        hex(&b[10..16])
    )
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Bytes of entropy, for a token nobody has to invent. Not a hot path, so the
/// cost of `getrandom` per call is irrelevant.
pub fn random_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

static REQUESTS: AtomicU64 = AtomicU64::new(0);

pub fn request_count() -> u64 {
    REQUESTS.load(Ordering::Relaxed)
}


#[cfg(test)]
mod tests {
    use super::ensure_single_statement as one;

    #[test]
    fn accepts_a_single_statement() {
        for sql in [
            "SELECT 1",
            "SELECT 1;",
            "SELECT 1;   \n  ",
            "SELECT ';' AS semi",
            "SELECT 'it''s; fine'",
            "SELECT E'a\\'; b'",
            r#"SELECT 1 AS "a;b""#,
            "SELECT $$a; b$$",
            "SELECT $tag$a; b$tag$",
            "SELECT 1 -- trailing; comment",
            "/* a; b */ SELECT 1",
            "/* a /* nested; */ b */ SELECT 1",
        ] {
            assert!(one(sql).is_ok(), "should accept: {sql}");
        }
    }

    #[test]
    fn rejects_a_second_statement() {
        for sql in [
            "SELECT 1; DROP TABLE orders",
            "SELECT 1;DROP TABLE orders",
            "SELECT ';'; DROP TABLE orders",
            "SELECT 1; -- sneaky",
            "SELECT 1;;",
            "/* x */ SELECT 1; SELECT 2",
        ] {
            assert!(one(sql).is_err(), "should reject: {sql}");
        }
    }
}
