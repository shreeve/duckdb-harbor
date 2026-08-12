// harbor — HTTP /sql for DuckDB: SQL in, NDJSON out, no driver.
//
// This is the extension entry point. harbor registers a small SQL surface
// and nothing else; every other capability lives on the HTTP side.
//
// What harbor deliberately does NOT do, and why:
//
//   - It does not serve the DuckDB UI. The upstream `ui` extension serves
//     itself in the same process: LOAD ui; CALL start_ui_server();
//   - It does not implement a client/server protocol for DuckDB clients.
//     The upstream `quack` extension does that: CALL quack_serve(...).
//   - It does not terminate TLS. Put Caddy (or any reverse proxy) in front.
//
// harbor's job is the audience neither of those serves: clients that do not
// embed DuckDB and just want to POST SQL and read JSON back.

use std::{
    error::Error,
    ffi::CString,
    io::Read,
    panic::AssertUnwindSafe,
    sync::{
        Arc, Condvar, Mutex, mpsc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use duckdb::{
    Connection, Result, ffi, params_from_iter,
    core::{DataChunkHandle, Inserter, LogicalTypeHandle, LogicalTypeId},
    duckdb_entrypoint_c_api,
    types::{TimeUnit, Value, ValueRef},
    vtab::{BindInfo, InitInfo, TableFunctionInfo, VTab},
};

use tiny_http::{Header, Method, Request, Response, Server};


// ==========================================================================
//
// The SQL surface
//
// ==========================================================================



/// Every function harbor exposes answers with one row and one VARCHAR
/// column. `init` tracks whether that row has been emitted; DuckDB calls
/// `func` until it returns an empty chunk.
struct OneShotInit {
    done: AtomicBool,
}

fn one_shot_init(_: &InitInfo) -> Result<OneShotInit, Box<dyn Error>> {
    Ok(OneShotInit { done: AtomicBool::new(false) })
}

/// Write `text` as the single output row, or close the stream if the row has
/// already gone out.
fn one_shot_emit(
    done: &AtomicBool,
    output: &mut DataChunkHandle,
    text: impl FnOnce() -> String,
) -> Result<(), Box<dyn Error>> {
    if done.swap(true, Ordering::Relaxed) {
        output.set_len(0);
        return Ok(());
    }
    output.flat_vector(0).insert(0, CString::new(text())?);
    output.set_len(1);
    Ok(())
}

fn varchar() -> LogicalTypeHandle {
    LogicalTypeHandle::from(LogicalTypeId::Varchar)
}

fn bigint() -> LogicalTypeHandle {
    LogicalTypeHandle::from(LogicalTypeId::Bigint)
}

fn boolean() -> LogicalTypeHandle {
    LogicalTypeHandle::from(LogicalTypeId::Boolean)
}

// ---------------------------------------------------------------------------
// harbor_version() -> VARCHAR
//
// The smoke-test surface. If this answers, the extension loaded and harbor's
// symbols are reachable.
// ---------------------------------------------------------------------------

struct HarborVersion;

impl VTab for HarborVersion {
    type InitData = OneShotInit;
    type BindData = ();

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        bind.add_result_column("version", varchar());
        Ok(())
    }

    fn init(init: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        one_shot_init(init)
    }

    fn func(func: &TableFunctionInfo<Self>, output: &mut DataChunkHandle) -> Result<(), Box<dyn Error>> {
        one_shot_emit(&func.get_init_data().done, output, || env!("CARGO_PKG_VERSION").to_string())
    }

    fn parameters() -> Option<Vec<LogicalTypeHandle>> {
        Some(vec![])
    }
}

// ---------------------------------------------------------------------------
// harbor_serve(bind := ..., port := ..., token := ..., workers := ..., log := ...)
//
// Starts the listener and returns immediately with the bound address, so the
// caller keeps a usable session. Loopback and a required token are the
// defaults because the alternative — a database exposed to the network with
// no credential — should never be one keystroke away.
// ---------------------------------------------------------------------------

struct HarborServe;

struct ServeConfig {
    bind: String,
    port: u16,
    token: Option<String>,
    workers: usize,
    /// One stderr line per request. Off by default: an access log is a cost
    /// paid on every request and a stream somebody has to rotate, and the
    /// caller that just wants a query endpoint should not inherit either.
    log: bool,
    /// Whether harbor minted the token rather than being given one, so the
    /// address it returns can show it the one time anyone will see it.
    generated: bool,
}

impl VTab for HarborServe {
    type InitData = OneShotInit;
    type BindData = ServeConfig;

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        bind.add_result_column("address", varchar());

        let host = bind
            .get_named_parameter("bind")
            .map(|v| v.to_string())
            .unwrap_or_else(|| "127.0.0.1".to_string());

        let port = bind.get_named_parameter("port").map(|v| v.to_int64()).unwrap_or(9495);
        if !(0..=65535).contains(&port) {
            return Err(format!("harbor_serve: port {port} is out of range").into());
        }

        // No token means harbor mints one and prints it, not that the
        // endpoint is open. `token := ''` is the explicit opt-out.
        let (token, generated) = match bind.get_named_parameter("token").map(|v| v.to_string()) {
            Some(t) if t.is_empty() => (None, false),
            Some(t) => (Some(t), false),
            None => (Some(random_token()), true),
        };

        let workers = bind
            .get_named_parameter("workers")
            .map(|v| v.to_int64())
            .unwrap_or(DEFAULT_MAX_INFLIGHT as i64);
        if workers < 1 {
            return Err("harbor_serve: workers must be at least 1".into());
        }

        let log = bind.get_named_parameter("log").map(|v| v.to_bool()).unwrap_or(false);

        Ok(ServeConfig {
            bind: host,
            port: port as u16,
            token,
            workers: workers as usize,
            log,
            generated,
        })
    }

    fn init(init: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        one_shot_init(init)
    }

    fn func(func: &TableFunctionInfo<Self>, output: &mut DataChunkHandle) -> Result<(), Box<dyn Error>> {
        let cfg = func.get_bind_data();
        if func.get_init_data().done.swap(true, Ordering::Relaxed) {
            output.set_len(0);
            return Ok(());
        }
        let address = start(&cfg.bind, cfg.port, cfg.token.clone(), cfg.workers, cfg.log)?;
        let mut text = format!("http://{address}");
        if cfg.generated {
            // The only chance to see a generated token is here; it is never
            // stored and never echoed again.
            text.push_str("  token=");
            text.push_str(cfg.token.as_deref().unwrap_or(""));
        }

        // Also to stderr, right now. Returning the address as a row is not
        // enough for the case that needs it most: the DuckDB CLI holds its
        // result output until the process exits, so a daemon started with
        // `duckdb -c 'CALL harbor_serve(...)'` printed the minted token only
        // once the server had already shut down — which is to say, never,
        // while it was of any use. stderr is unbuffered and goes to the
        // journal, so this is the line an operator actually sees.
        eprintln!("harbor: serving on {text}");
        output.flat_vector(0).insert(0, CString::new(text)?);
        output.set_len(1);
        Ok(())
    }

    fn parameters() -> Option<Vec<LogicalTypeHandle>> {
        Some(vec![])
    }

    fn named_parameters() -> Option<Vec<(String, LogicalTypeHandle)>> {
        Some(vec![
            ("bind".to_string(), varchar()),
            ("port".to_string(), bigint()),
            ("token".to_string(), varchar()),
            ("workers".to_string(), bigint()),
            ("log".to_string(), boolean()),
        ])
    }
}

// ---------------------------------------------------------------------------
// harbor_stop() — stop the listener, drain the workers, CHECKPOINT
// ---------------------------------------------------------------------------

struct HarborStop;

impl VTab for HarborStop {
    type InitData = OneShotInit;
    type BindData = ();

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        bind.add_result_column("address", varchar());
        Ok(())
    }

    fn init(init: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        one_shot_init(init)
    }

    fn func(func: &TableFunctionInfo<Self>, output: &mut DataChunkHandle) -> Result<(), Box<dyn Error>> {
        if func.get_init_data().done.swap(true, Ordering::Relaxed) {
            output.set_len(0);
            return Ok(());
        }
        let address = stop()?;
        output.flat_vector(0).insert(0, CString::new(address)?);
        output.set_len(1);
        Ok(())
    }

    fn parameters() -> Option<Vec<LogicalTypeHandle>> {
        Some(vec![])
    }
}

// ---------------------------------------------------------------------------
// harbor_wait() — block until the server stops
//
// This is what turns a DuckDB CLI invocation into a daemon: run harbor_serve,
// then harbor_wait, and the process stays up serving until something calls
// harbor_stop.
// ---------------------------------------------------------------------------

struct HarborWait;

impl VTab for HarborWait {
    type InitData = OneShotInit;
    type BindData = ();

    fn bind(bind: &BindInfo) -> Result<Self::BindData, Box<dyn Error>> {
        bind.add_result_column("address", varchar());
        Ok(())
    }

    fn init(init: &InitInfo) -> Result<Self::InitData, Box<dyn Error>> {
        one_shot_init(init)
    }

    fn func(func: &TableFunctionInfo<Self>, output: &mut DataChunkHandle) -> Result<(), Box<dyn Error>> {
        if func.get_init_data().done.swap(true, Ordering::Relaxed) {
            output.set_len(0);
            return Ok(());
        }
        let address = wait()?;
        output.flat_vector(0).insert(0, CString::new(address)?);
        output.set_len(1);
        Ok(())
    }

    fn parameters() -> Option<Vec<LogicalTypeHandle>> {
        Some(vec![])
    }
}

#[duckdb_entrypoint_c_api]
pub unsafe fn extension_entrypoint(con: Connection) -> Result<(), Box<dyn Error>> {
    con.register_table_function::<HarborVersion>("harbor_version")?;
    con.register_table_function::<HarborServe>("harbor_serve")?;
    con.register_table_function::<HarborStop>("harbor_stop")?;
    con.register_table_function::<HarborWait>("harbor_wait")?;

    // Open the worker connections. This has to happen here, inside the load
    // callback: the database handle an extension is given does not outlive
    // it, so a connection opened later fails — see the note above POOL.
    open_pool(con)?;

    Ok(())
}

// ==========================================================================
//
// The HTTP server
//
// ==========================================================================


// The HTTP side of harbor.
//
// Shape (deliberately small):
//
//   POST /sql     run one statement, stream the NDJSON envelope back
//   GET  /health  liveness, no auth
//
// The envelope is the one thing that must not drift from the v1 harbor,
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
// in-flight statements; a connection past that waits for a worker to come
// free. It does not wait in the kernel accept backlog — tiny_http accepts
// eagerly on its own thread and gives each connection an OS thread, so
// connection count, not worker count, is what a flood actually costs.


// duckdb-rs keeps the raw `duckdb_logical_type` private, and two details are
// reachable only through the C API: an ARRAY's length and an ENUM's value
// list. `LogicalTypeHandle` is a single-field newtype around that pointer, so
// a copy of its bytes is the pointer. The assertion turns a layout change in
// duckdb-rs into a compile error instead of a crash at runtime.
const _: () = assert!(
    std::mem::size_of::<LogicalTypeHandle>() == std::mem::size_of::<ffi::duckdb_logical_type>()
);

/// Borrow the handle's pointer. The handle keeps ownership; the result must
/// not outlive it and must not be destroyed.
fn raw_type(ty: &LogicalTypeHandle) -> ffi::duckdb_logical_type {
    unsafe { std::mem::transmute_copy(ty) }
}

fn array_size(ty: &LogicalTypeHandle) -> u64 {
    unsafe { ffi::duckdb_array_type_array_size(raw_type(ty)) }
}

fn enum_values(ty: &LogicalTypeHandle) -> Vec<String> {
    unsafe {
        let handle = raw_type(ty);
        let count = ffi::duckdb_enum_dictionary_size(handle) as usize;
        (0..count)
            .map(|i| {
                let ptr = ffi::duckdb_enum_dictionary_value(handle, i as u64);
                if ptr.is_null() {
                    return String::new();
                }
                let value = std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned();
                ffi::duckdb_free(ptr as *mut std::ffi::c_void);
                value
            })
            .collect()
    }
}

/// Render an identifier the way DuckDB does inside a type string: bare when it
/// is a simple lowercase identifier and not a keyword, double-quoted
/// otherwise, with embedded quotes doubled.
fn quote_identifier(name: &str) -> String {
    let simple = !name.is_empty()
        && name.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if simple && KEYWORDS.binary_search(&name).is_err() {
        return name.to_string();
    }
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Bounded number of statements executing at once. Connections may greatly
/// exceed this; queries should not.
const DEFAULT_MAX_INFLIGHT: usize = 6;

/// Largest request body we will read, declared or delivered. A statement is
/// text, and a megabyte of it is already pathological — the limit sits well
/// above that so a generous `params` array is never the thing that fails.
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
    workers: Vec<JoinHandle<Option<Connection>>>,
    addr: String,
}

/// Open every connection harbor will need. Called once, from the extension
/// entrypoint, and only there — see the note above on why later is too late.
fn open_pool(con: Connection) -> Result<(), String> {
    let mut pool = POOL.lock().unwrap();

    // Once per process, not once per load. POOL and CONTROL are process-wide,
    // but the entrypoint runs once per *database instance* — a host that opens
    // two DuckDB databases and loads harbor into both would otherwise append
    // eight more connections to the same vector and overwrite CONTROL with the
    // second database's. `start()` drains from the tail, so harbor_serve on the
    // first instance would then serve the second one's data, and the shutdown
    // CHECKPOINT would run against whichever loaded last. Refusing is the only
    // honest answer: harbor is a process singleton and cannot serve two.
    if !pool.is_empty() {
        return Err(
            "harbor is already loaded in this process and serves a single database; \
             loading it into a second one is not supported"
                .to_string(),
        );
    }

    for _ in 0..POOL_SIZE {
        pool.push(con.try_clone().map_err(|e| format!("harbor: {e}"))?);
    }
    *CONTROL.lock().unwrap() = Some(con);
    Ok(())
}

// ---------------------------------------------------------------------------
// start / stop / wait
// ---------------------------------------------------------------------------

fn start(
    bind: &str,
    port: u16,
    token: Option<String>,
    workers: usize,
    log: bool,
) -> Result<String, String> {
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
    // Say so rather than quietly serving with fewer. workers is capped by the
    // connection pool, which is fixed at POOL_SIZE, so `workers := 32` gets 8 —
    // and someone who raised it to fix a throughput problem deserves to know
    // that the number they set is not the number they got.
    let requested = workers;
    let workers = workers.clamp(1, pool.len());
    if requested > workers {
        eprintln!(
            "harbor: workers={requested} exceeds the {workers}-connection pool; serving with {workers}"
        );
    }
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
                .spawn(move || worker(server, stop, token, conn, log))
                .map_err(|e| e.to_string())?,
        );
    }

    *STOPPED.0.lock().unwrap() = false;
    *running = Some(Running { server, stop, workers: handles, addr: addr.clone() });
    Ok(addr)
}

fn stop() -> Result<String, String> {
    // Held for the whole of the shutdown, not just the take(). Releasing it
    // here — which `RUNNING.lock().unwrap().take()` as a statement does, since
    // the guard is a temporary — leaves a window in which RUNNING is None while
    // the listener is still bound and the workers are still draining. A
    // harbor_serve arriving in that window sees no server, takes whichever
    // connections happen to be back in the pool, and then fails to bind a port
    // the old listener has not released yet.
    let mut running = RUNNING.lock().unwrap();
    let Some(r) = running.take() else {
        return Err("harbor is not serving".to_string());
    };
    r.stop.store(true, Ordering::SeqCst);
    r.server.unblock();

    // Workers hand their connection back as they exit, so a later
    // harbor_serve has a pool to draw from. A panicked worker forfeits its
    // connection rather than taking the shutdown down with it.
    let mut pool = POOL.lock().unwrap();
    for h in r.workers {
        if let Ok(Some(conn)) = h.join() {
            pool.push(conn);
        }
    }
    drop(pool);

    // Fold the WAL back into the database file so the next open needs no
    // replay. This succeeds when harbor_stop is called from an ordinary
    // session, and fails harmlessly when it is called from the signal handler
    // while harbor_wait is still blocked: that blocked call is itself an open
    // transaction older than every write, and DuckDB will not checkpoint past
    // one. The daemon path covers that case by running CHECKPOINT after
    // harbor_wait returns — see bin/duckdb-harbor.
    if let Some(c) = CONTROL.lock().unwrap().as_ref() {
        let _ = c.execute_batch("CHECKPOINT");
    }

    let (lock, cv) = &STOPPED;
    *lock.lock().unwrap() = true;
    cv.notify_all();
    drop(running);
    Ok(r.addr)
}

/// Turn SIGTERM and SIGINT into a clean `stop()`.
///
/// Registered from `wait()` and nowhere else. `wait()` is what makes the
/// process a daemon — nothing else is going to happen on the main thread —
/// so that is the one moment where taking over the signals is harbor's call
/// to make. In an ordinary interactive session the CLI keeps its own Ctrl-C,
/// which cancels a query rather than shutting the database down.
///
/// Without this, a `kill` runs the default handler: the process dies with the
/// WAL unfolded and the next open has to replay it.
#[cfg(unix)]
fn install_signal_handler() {
    use signal_hook::{
        consts::{SIGINT, SIGTERM},
        iterator::Signals,
    };

    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }

    if let Ok(mut signals) = Signals::new([SIGTERM, SIGINT]) {
        let _ = thread::Builder::new().name("harbor-signals".to_string()).spawn(move || {
            // No `break`. Breaking out ends the thread, which drops `Signals`
            // and restores the default disposition — so the *second* signal did
            // exactly what this function exists to prevent: killed the process
            // with the WAL unfolded. A supervisor escalating after a timeout,
            // or an impatient second Ctrl-C, both land in that window, and the
            // launcher's CHECKPOINT runs after wait() returns — precisely then.
            // stop() is idempotent enough to call again: the second call finds
            // RUNNING empty and returns an error nobody reads.
            for _ in signals.forever() {
                // stop() drains the workers and checkpoints, then wakes
                // wait(), which lets the main thread exit normally.
                let _ = stop();
            }
        });
    }
}

#[cfg(not(unix))]
fn install_signal_handler() {}

/// Block until the server stops. Returns the address it was serving on.
fn wait() -> Result<String, String> {
    let addr = match RUNNING.lock().unwrap().as_ref() {
        Some(r) => r.addr.clone(),
        None => return Err("harbor is not serving".to_string()),
    };
    install_signal_handler();
    let (lock, cv) = &STOPPED;
    let mut stopped = lock.lock().unwrap();
    while !*stopped {
        stopped = cv.wait(stopped).unwrap();
    }
    Ok(addr)
}

// ---------------------------------------------------------------------------
// Request handling
// ---------------------------------------------------------------------------

/// One HTTP worker. It owns the socket side only; the DuckDB connection lives
/// on a dedicated executor thread it starts and hands work to.
///
/// The split is what makes keep-alive possible. tiny_http will frame a
/// response of unknown length itself — chunked, connection reusable — but
/// only if it is handed a `Read` to pull from. A query cannot be that `Read`:
/// the rows come from a borrow chain rooted in a `Connection` that is not
/// `Sync`. Putting the connection on its own thread and passing byte chunks
/// through a bounded channel gives tiny_http its reader and keeps the query
/// streaming.
///
/// Before this, harbor took the raw socket with `into_writer()` and wrote the
/// framing by hand, which forces `Connection: close`. That costs a client one
/// ephemeral port per request, held for the TIME_WAIT interval — about
/// 16k ports over 30s on macOS, so a single client hitting a few thousand
/// requests per second runs out of ports in seconds and starts seeing
/// `Can't assign requested address`. Reusing the connection removes the cost
/// entirely.
fn worker(
    server: Arc<Server>,
    stop: Arc<AtomicBool>,
    token: Arc<Option<String>>,
    conn: Connection,
    log: bool,
) -> Option<Connection> {
    // Rendezvous: a worker never has more than one statement outstanding, so
    // there is nothing to queue here.
    let (jobs_tx, jobs_rx) = mpsc::sync_channel::<Job>(0);
    let executor = thread::Builder::new()
        .name("harbor-exec".to_string())
        .spawn(move || execute_jobs(conn, jobs_rx))
        .ok()?;

    while !stop.load(Ordering::SeqCst) {
        // A timeout rather than a blocking recv, so `unblock()` is not the
        // only way out and a worker cannot wedge on shutdown.
        match server.recv_timeout(Duration::from_millis(200)) {
            // A worker whose executor has died must leave the accept loop. All
            // workers pull from one shared queue, so one that answers instantly
            // — which is what a worker with no executor does, 503 by return —
            // wins races against every worker still doing real work, and
            // absorbs a growing share of the traffic. The server would look
            // mostly down while /health kept answering 200.
            Ok(Some(req)) => {
                if !handle(req, &jobs_tx, token.as_ref().as_deref(), log) {
                    break;
                }
            }
            Ok(None) => continue,
            Err(_) => break,
        }
    }

    drop(jobs_tx);
    executor.join().ok()
}

/// Returns false when the executor behind `jobs` is gone, which is the one
/// condition the caller must act on rather than merely report.
fn handle(mut req: Request, jobs: &mpsc::SyncSender<Job>, token: Option<&str>, log: bool) -> bool {
    let path = req.url().split('?').next().unwrap_or("/").to_string();
    let method = req.method().clone();

    // Only when logging. A clock read and a peer-address format are small, but
    // they are paid on every request by every caller, including the ones that
    // asked for a query endpoint and nothing else.
    let started = log.then(Instant::now);
    let peer = match log {
        true => req.remote_addr().map_or_else(|| "-".to_string(), |a| a.ip().to_string()),
        false => String::new(),
    };

    // Each arm reports the status it sent, so the log line below is written in
    // one place instead of at all seven `respond` calls. The SQL text is not
    // logged: it is the request body, it can be enormous, and on this endpoint
    // it is as likely to hold customer data as anything else in the database.
    let (keep_going, status) = match (&method, path.as_str()) {
        // Liveness is unauthenticated on purpose: a load balancer should not
        // need a credential to learn whether the process is up, and the
        // answer reveals nothing.
        (Method::Get, "/health") => {
            let _ = req.respond(json_response(200, r#"{"status":"ok"}"#));
            (true, 200)
        }
        (Method::Post, "/sql") => {
            if !authorized(&req, token) {
                let _ = req.respond(error_response(401, "unauthorized", "missing or invalid bearer token"));
                (true, 401)
            }
            // Check the declared length before touching the reader. `take()`
            // bounds what harbor buffers but not what the client may declare,
            // and tiny_http drains whatever is left undelivered when the
            // request is dropped — with a single `vec![0; remaining]`. So a
            // client that declares 600 MB and sends 9 MB costs the process a
            // 600 MB zeroed allocation it never asked for; the declared length
            // is attacker-chosen and unbounded. Refusing here means the
            // allocation never happens.
            else if let Some(n) = req.body_length().filter(|n| *n > MAX_BODY) {
                let _ = req.respond(error_response(
                    413,
                    "body_too_large",
                    &format!("body is {n} bytes; the limit is {MAX_BODY}"),
                ));
                (true, 413)
            } else {
                let mut body = String::new();
                if req.as_reader().take(MAX_BODY as u64).read_to_string(&mut body).is_err() {
                    let _ = req.respond(error_response(400, "bad_request", "body is not valid UTF-8"));
                    (true, 400)
                } else {
                    run_sql(req, jobs, &body)
                }
            }
        }
        _ => {
            let _ = req.respond(error_response(404, "not_found", "no such endpoint"));
            (true, 404)
        }
    };

    // After respond(), not before: tiny_http writes the body from the reader
    // inside that call, so for a streamed result this elapsed time covers the
    // whole query and the whole transfer rather than just the headers.
    if let Some(t) = started {
        eprintln!(
            "harbor: {} {peer} {} {path} {status} {}ms",
            utc_now(),
            method.as_str(),
            t.elapsed().as_millis()
        );
    }
    keep_going
}

/// UTC, RFC 3339, seconds resolution: `2026-08-12T04:31:07Z`.
///
/// No date crate: an access log is unreadable without a timestamp — nothing in
/// front of harbor supplies one, since launchd and a plain `2>>file` redirect
/// both pass stderr through verbatim — but one format in one timezone is not
/// worth a dependency when `civil_from_days` is already here for DATE.
fn utc_now() -> String {
    let secs =
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let tod = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}


fn authorized(req: &Request, token: Option<&str>) -> bool {
    let Some(expected) = token else { return true };

    // Exactly one Authorization header, or none of them count. Taking the first
    // and ignoring the rest means harbor and anything in front of it can read
    // the same request differently, which is how a proxy and an origin end up
    // disagreeing about who the caller is. Duplicates are not something a
    // correct client sends, so refusing them costs nothing.
    let mut found = req.headers().iter().filter(|h| h.field.equiv("Authorization"));
    let Some(h) = found.next() else { return false };
    if found.next().is_some() {
        return false;
    }

    // RFC 7235 makes the scheme case-insensitive; the value after it is not.
    let value = h.value.as_str();
    let split = value.find(' ').unwrap_or(value.len());
    let (scheme, rest) = value.split_at(split);
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return false;
    }
    let Some(presented) = rest.strip_prefix(' ') else { return false };
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
                //
                // That `e` has to be a token on its own. Testing only the byte
                // before the quote is not enough, and the difference is a hole
                // rather than a nicety: DuckDB needs no space between a keyword
                // and a literal, so LIKE', ILIKE', ESCAPE', date' and time' all
                // end in `e`. Reading one of those as an escape string makes the
                // scanner honour a backslash, skip the byte after it — the real
                // closing quote — and swallow every `;` that follows. That is a
                // second statement smuggled past this function, which is the one
                // thing it exists to prevent.
                let standalone_e = |j: usize| {
                    (b[j] | 0x20) == b'e'
                        && (j == 0 || !(b[j - 1].is_ascii_alphanumeric() || b[j - 1] == b'_' || b[j - 1] >= 0x80))
                };
                let escapes = q == b'\'' && i > 0 && standalone_e(i - 1);
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
                    // A tag may not start with a digit: `$1$` is a bind
                    // parameter to DuckDB, not the opening of a string. Reading
                    // it as one would let `$1$; DROP TABLE t; $1$` hide a
                    // terminator the two lexers disagree about.
                    Some(end) if b[end] == b'$' && !b[i + 1].is_ascii_digit() => {
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
                // Anything after a terminator is a second statement, even if
                // it is only a comment — harbor has no reason to accept it.
                //
                // Decided once, here, rather than re-tested on every byte that
                // follows. The earlier form rescanned the whole tail each time
                // round the loop, which is Θ(n²): a request of `SELECT 1;` plus
                // trailing whitespace, still inside the 8 MiB body limit, cost
                // hours of CPU on the worker thread that read it.
                return if b[i + 1..].iter().all(|c| c.is_ascii_whitespace()) {
                    Ok(())
                } else {
                    Err("only one statement per request".to_string())
                };
            }
            _ => i += 1,
        }
    }
    Ok(())
}

/// One unit of work for an executor thread.
struct Job {
    sql: String,
    params: Vec<Value>,
    /// Answered exactly once, before any body byte is produced. `Err` means
    /// nothing has been written yet, so the worker can still pick a status
    /// code — which is the whole reason preparation is reported separately
    /// from streaming.
    ready: mpsc::SyncSender<Result<(), String>>,
    /// Body bytes, in envelope-line batches. Bounded, so a slow client
    /// applies backpressure to the query instead of buffering the result.
    body: mpsc::SyncSender<Vec<u8>>,
}

/// How many body batches may be in flight before the query has to wait.
const BODY_QUEUE: usize = 4;

/// Returns (keep serving, status sent). The first is false when the executor is
/// gone; see `handle`, which also writes the log line from the second.
fn run_sql(req: Request, jobs: &mpsc::SyncSender<Job>, body: &str) -> (bool, u16) {
    let parsed = match parse_request(body) {
        Ok(p) => p,
        Err(e) => {
            let _ = req.respond(error_response(400, "bad_request", &e));
            return (true, 400);
        }
    };

    if let Err(e) = ensure_single_statement(&parsed.sql) {
        let _ = req.respond(error_response(400, "bad_request", &e));
        return (true, 400);
    }

    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), String>>(1);
    let (body_tx, body_rx) = mpsc::sync_channel::<Vec<u8>>(BODY_QUEUE);
    let job = Job { sql: parsed.sql, params: parsed.params, ready: ready_tx, body: body_tx };

    if jobs.send(job).is_err() {
        let _ = req.respond(error_response(503, "unavailable", "harbor is shutting down"));
        return (false, 503);
    }

    match ready_rx.recv() {
        Ok(Ok(())) => {
            // data_length: None makes tiny_http chunk the body and keep the
            // connection alive.
            let headers = vec![
                Header::from_bytes(&b"Content-Type"[..], &b"application/x-ndjson"[..]).unwrap(),
                Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]).unwrap(),
            ];
            let _ = req.respond(Response::new(200.into(), headers, ChannelReader::new(body_rx), None, None));
            (true, 200)
        }
        Ok(Err(message)) => {
            let _ = req.respond(error_response(400, "sql_error", &message));
            (true, 400)
        }
        Err(_) => {
            let _ = req.respond(error_response(500, "internal", "the executor thread is gone"));
            (false, 500)
        }
    }
}

/// Whether a statement could leave a transaction open behind it.
///
/// Only transaction-control statements can, because a statement that runs in
/// autocommit commits or rolls back its own implicit transaction as it
/// finishes. So the reset before the next job is only needed after one of
/// these — or after a job that ended abnormally, which the caller tracks
/// separately.
///
/// Fail-safe by construction: this answers true for anything it does not
/// recognise, so a statement form nobody thought of costs one `ROLLBACK`
/// rather than leaving a transaction open. Getting it wrong in the other
/// direction is what took connections out of service permanently.
fn may_leave_transaction_open(sql: &str) -> bool {
    let b = sql.as_bytes();
    let mut i = 0;
    // Skip leading whitespace and comments to reach the first real token.
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if b[i..].starts_with(b"--") {
            i = b[i..].iter().position(|&c| c == b'\n').map_or(b.len(), |p| i + p + 1);
        } else if b[i..].starts_with(b"/*") {
            let mut depth = 1;
            i += 2;
            while i < b.len() && depth > 0 {
                if b[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if b[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        } else {
            break;
        }
    }
    let start = i;
    while i < b.len() && (b[i].is_ascii_alphabetic() || b[i] == b'_') {
        i += 1;
    }
    if i == start {
        return true;
    }
    let word = sql[start..i].to_ascii_uppercase();
    // Statement kinds that run under autocommit and settle themselves. Anything
    // absent from this list — BEGIN, START, COMMIT, ROLLBACK, ABORT, END, and
    // whatever DuckDB adds next — takes the safe path.
    !matches!(
        word.as_str(),
        "SELECT" | "WITH" | "FROM" | "VALUES" | "TABLE"
            | "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "TRUNCATE"
            | "CREATE" | "DROP" | "ALTER" | "COMMENT"
            | "COPY" | "EXPORT" | "IMPORT"
            | "ATTACH" | "DETACH" | "USE"
            | "PRAGMA" | "SET" | "RESET" | "CHECKPOINT" | "ANALYZE" | "VACUUM"
            | "EXPLAIN" | "DESCRIBE" | "SHOW" | "SUMMARIZE" | "PIVOT" | "UNPIVOT"
            | "CALL" | "PREPARE" | "EXECUTE" | "DEALLOCATE"
            | "INSTALL" | "LOAD"
    )
}

/// Return a connection to autocommit before anything else runs on it.
///
/// Pooled connections are handed out per request and are never pinned to a
/// client, so a transaction cannot usefully span two requests — but a client
/// can still send `BEGIN`, and DuckDB will honour it. That leaves the
/// connection inside a transaction for whoever gets it next, and if the
/// transaction has already failed, every subsequent statement on it comes back
/// `Current transaction is aborted` for the life of the process. One request
/// from one careless client would otherwise take a worker out of service
/// permanently, and with a pool of eight it takes eight such requests to stop
/// the server answering at all.
///
/// Rolling back is the only correct choice here: the client that opened the
/// transaction has no way to commit it, since its next request will land on a
/// different connection.
///
/// Unconditional, and it has to be. This used to run only when a job had ended
/// abnormally or when `Connection::is_autocommit()` reported a transaction
/// open, on the reasoning that an ordinary request should not pay for an extra
/// statement. But `is_autocommit()` in `duckdb-rs` 1.10505.0 is a stub — the
/// whole body is `true` — so that half of the condition never fired and the
/// abnormal-exit flag was doing all of the work. An open transaction left by a
/// plain `BEGIN` survived on the connection until some later job on that same
/// connection happened to end badly. It is observable: send `BEGIN` once per
/// worker and the next request on each of those connections fails with
/// `cannot start a transaction within a transaction`.
///
/// A `ROLLBACK` on a connection that has nothing to roll back is cheap, and far
/// cheaper than the failure it prevents — an open transaction also blocks
/// `CHECKPOINT`, including the one `stop()` runs, which is how a WAL goes
/// unfolded.
fn reset_transaction(conn: &Connection) {
    let _ = conn.execute_batch("ROLLBACK");
}

/// The DuckDB side. Owns one connection for the life of the server and runs
/// one statement at a time; concurrency comes from there being several of
/// these, not from any one of them interleaving work.
fn execute_jobs(conn: Connection, jobs: mpsc::Receiver<Job>) -> Connection {
    // Set by the previous job when it could have left a transaction open: a
    // transaction-control statement, or any exit other than running to
    // completion. Resetting unconditionally is also correct, and was what this
    // did for a while, but it puts a `ROLLBACK` in front of every request —
    // measurably, about 20% of throughput at 16 clients. The flag has to be set
    // from something real, though: it used to consult
    // `Connection::is_autocommit()`, which duckdb-rs hardcodes to `true`, so
    // that half of the condition never fired at all.
    let mut needs_reset = false;
    for job in jobs {
        // Before, not after: a job can leave the loop by several paths, and
        // this way none of them can skip the reset.
        if needs_reset {
            reset_transaction(&conn);
        }

        let Job { sql, params, ready, body } = job;
        let started = Instant::now();

        // Decided from the statement text before it runs, then widened below by
        // any path that ends the job early.
        needs_reset = may_leave_transaction_open(&sql);

        let stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(e) => {
                needs_reset = true;
                let _ = ready.send(Err(e.to_string()));
                continue;
            }
        };
        let mut stmt = stmt;
        let mut rows = match stmt.query(params_from_iter(params.iter())) {
            Ok(r) => r,
            Err(e) => {
                needs_reset = true;
                let _ = ready.send(Err(e.to_string()));
                continue;
            }
        };

        // Column metadata has to be captured now: `Rows` hands back `None`
        // from `as_ref()` once the result is exhausted.
        let (names, types) = match rows.as_ref() {
            Some(s) => {
                let n = s.column_count();
                let names: Vec<String> =
                    (0..n).map(|i| s.column_name(i).cloned().unwrap_or_default()).collect();
                let types: Vec<LogicalTypeHandle> = (0..n).map(|i| s.column_logical_type(i)).collect();
                (names, types)
            }
            None => (Vec::new(), Vec::new()),
        };

        // duckdb-rs does not return an error for a column type it has no
        // decoder for — it panics. `SELECT TIME_NS '...'` reaches an
        // `unreachable!` in its row.rs. Catching that mid-stream is possible
        // (and is done below), but by then the 200 and the headers are gone
        // and the client can only be told inside the body. Refusing here,
        // before anything is sent, is the difference between a 400 that says
        // what is wrong and a 200 that appears to have returned no rows.
        if let Some((name, bad)) = names
            .iter()
            .zip(&types)
            .find(|(_, t)| matches!(t.try_id(), Ok(LogicalTypeId::TimeNs)))
        {
            // Name the column, not the type. Substituting the type into both
            // slots produced "Cast it — TIME_NS::VARCHAR", which reads as
            // casting the type rather than the thing that has it.
            let _ = ready.send(Err(format!(
                "harbor cannot encode {} columns: the DuckDB Rust client has no \
                 decoder for this type. Cast it — {}::VARCHAR, or {}::TIME for \
                 microsecond precision — and the value comes back intact.",
                type_name(bad),
                name,
                name
            )));
            needs_reset = true;
            continue;
        }

        if ready.send(Ok(())).is_err() {
            needs_reset = true;
            continue;
        }

        let mut buf = String::with_capacity(FLUSH_AT + 8192);
        buf.push_str(r#"{"type":"schema","columns":["#);
        for (i, (name, ty)) in names.iter().zip(&types).enumerate() {
            if i > 0 {
                buf.push(',');
            }
            emit_column_schema(&mut buf, Some(name), ty);
        }
        buf.push_str("]}\n");

        let mut count: u64 = 0;
        let mut gone = false;
        loop {
            match rows.next() {
                Ok(Some(row)) => {
                    // The safety net behind the pre-flight check above. A
                    // panic in here would otherwise kill this executor
                    // thread, and the damage is worse than one failed query:
                    // the client sees 200 with an empty body — success, no
                    // rows — and the connection never returns to the pool, so
                    // POOL_SIZE such queries take the server out of service.
                    // Built into a separate buffer so a half-written row is
                    // discarded rather than shipped.
                    let mut cells = String::new();
                    let encoded = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    for (i, ty) in types.iter().enumerate() {
                        if i > 0 {
                            cells.push(',');
                        }
                        // A SQLNULL column — `SELECT NULL AS x`, with no cast
                        // to give it a type — holds nothing but NULL, and
                        // duckdb-rs has no decoder for it: reading the value
                        // panics, and the whole result dies with it. The value
                        // is not in any doubt, so answer it here instead of
                        // refusing a row whose contents are already known.
                        //
                        // Only DuckDB v2 gets here. v1.5.5 coerces an untyped
                        // NULL to INTEGER before harbor ever sees the column,
                        // so the same query took the ordinary path there — the
                        // kind of difference that only shows up when harbor is
                        // actually run against v2.
                        if matches!(ty.try_id(), Ok(LogicalTypeId::SqlNull)) {
                            cells.push_str("null");
                            continue;
                        }
                        match row.get_ref(i) {
                            // A UNION's tag says which member is set, and
                            // `Value` drops it — union_value(a := 2) and
                            // union_value(b := 2) would be indistinguishable.
                            // The tag is still on the arrow array underneath.
                            Ok(v) => {
                                let tag = union_tag(&v);
                                emit_tagged(&mut cells, tag, &Value::from(v), Some(ty));
                            }
                            Err(_) => cells.push_str("null"),
                        }
                    }
                    }));

                    if encoded.is_err() {
                        // The headers are long gone, so this can only be said
                        // in the stream — but it is said, rather than the
                        // client being left to infer it from a short result.
                        buf.push_str(r#"{"type":"error","code":"unsupported_type","message":"#);
                        push_json_string(
                            &mut buf,
                            "harbor cannot encode a value in this result: the DuckDB Rust \
                             client has no decoder for one of its column types. The query \
                             ran; the value cannot be represented. Cast the column to \
                             VARCHAR to see it.",
                        );
                        buf.push_str("}\n");
                        let _ = body.send(std::mem::take(&mut buf).into_bytes());
                        gone = true;
                        break;
                    }

                    buf.push_str(r#"{"type":"row","values":["#);
                    buf.push_str(&cells);
                    buf.push_str("]}\n");
                    count += 1;
                    if buf.len() >= FLUSH_AT {
                        // A send failure means the client hung up. Abandon the
                        // query rather than finish computing a result nobody
                        // will read.
                        if body.send(std::mem::take(&mut buf).into_bytes()).is_err() {
                            gone = true;
                            break;
                        }
                        buf = String::with_capacity(FLUSH_AT + 8192);
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    // Mid-stream failures cannot change the status code — the
                    // headers are long gone. Say so in the stream, so a client
                    // never mistakes a truncated result for a complete one.
                    buf.push_str(r#"{"type":"error","code":"sql_error","message":"#);
                    push_json_string(&mut buf, &e.to_string());
                    buf.push_str("}\n");
                    let _ = body.send(std::mem::take(&mut buf).into_bytes());
                    gone = true;
                    break;
                }
            }
        }

        // An abandoned or failed stream is the case that poisons a connection.
        needs_reset = needs_reset || gone;

        if !gone {
            buf.push_str(&format!(
                r#"{{"type":"end","rowCount":{},"timeMs":{}}}"#,
                count,
                started.elapsed().as_millis()
            ));
            buf.push('\n');
            let _ = body.send(buf.into_bytes());
        }
    }
    // And once more on the way out, so a connection going back to the pool for
    // the next harbor_serve is clean too. Unconditional here: this runs once
    // per server lifetime, so the extra statement costs nothing.
    reset_transaction(&conn);
    conn
}

/// Adapts the body channel to the `Read` tiny_http wants. Returning `Ok(0)`
/// when the sender is dropped is what ends the chunked response.
struct ChannelReader {
    rx: mpsc::Receiver<Vec<u8>>,
    current: Vec<u8>,
    pos: usize,
}

impl ChannelReader {
    fn new(rx: mpsc::Receiver<Vec<u8>>) -> Self {
        Self { rx, current: Vec::new(), pos: 0 }
    }
}

impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.pos >= self.current.len() {
            match self.rx.recv() {
                Ok(next) => {
                    self.current = next;
                    self.pos = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let n = (self.current.len() - self.pos).min(out.len());
        out[..n].copy_from_slice(&self.current[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

// ---------------------------------------------------------------------------
// Responses
//
// ---------------------------------------------------------------------------

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
            out.push_str(r#","lossless":true,"arrayLength":"#);
            out.push_str(&array_size(ty).to_string());
            out.push_str(r#","child":"#);
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
        LogicalTypeId::Map => {
            // A SQL MAP has no JSON counterpart — its keys need not be strings
            // — so values go out as pairs and the encoding says so.
            out.push_str(r#","lossless":true,"keyType":"#);
            emit_column_schema(out, None, &ty.child(0));
            out.push_str(r#","valueType":"#);
            emit_column_schema(out, None, &ty.child(1));
            out.push_str(r#","encoding":"pairs""#);
        }
        LogicalTypeId::Union => {
            out.push_str(r#","lossless":true,"members":["#);
            for i in 0..ty.num_children() {
                if i > 0 {
                    out.push(',');
                }
                emit_column_schema(out, Some(&ty.child_name(i)), &ty.child(i));
            }
            out.push(']');
        }
        LogicalTypeId::Enum => {
            out.push_str(r#","lossless":true,"values":["#);
            for (i, value) in enum_values(ty).iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                push_json_string(out, value);
            }
            out.push(']');
        }
        // TIME WITH TIME ZONE is the one type harbor cannot carry
        // losslessly: duckdb-rs decodes it to a local time and drops the UTC
        // offset before harbor ever sees the value, so the offset cannot be
        // recovered. Saying so is better than emitting a time that silently
        // means something else.
        LogicalTypeId::TimeTZ => out.push_str(r#","lossless":false,"encoding":"time-offset-dropped""#),
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
            | Timestamp
            | TimestampS
            | TimestampMs
            | TimestampNs
            | TimestampTZ
            | Interval
            | Blob
            | Bit
            // Lossless because it goes out as its decimal digits — a string
            // when it exceeds what a double holds, so no precision is lost on
            // the way through a JSON parser.
            | Bignum
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
        Array => format!("{}[{}]", type_name(&ty.child(0)), array_size(ty)),
        Enum => {
            let values: Vec<String> =
                enum_values(ty).iter().map(|v| format!("'{}'", v.replace('\'', "''"))).collect();
            format!("ENUM({})", values.join(", "))
        }
        Struct => {
            let fields: Vec<String> = (0..ty.num_children())
                .map(|i| format!("{} {}", quote_identifier(&ty.child_name(i)), type_name(&ty.child(i))))
                .collect();
            format!("STRUCT({})", fields.join(", "))
        }
        Map => format!("MAP({}, {})", type_name(&ty.child(0)), type_name(&ty.child(1))),
        Union => {
            let members: Vec<String> = (0..ty.num_children())
                .map(|i| format!("{} {}", quote_identifier(&ty.child_name(i)), type_name(&ty.child(i))))
                .collect();
            format!("UNION({})", members.join(", "))
        }
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

/// The name of the member a UNION value actually holds, if this is one.
fn union_tag(v: &ValueRef<'_>) -> Option<String> {
    use duckdb::arrow::{array::{Array, UnionArray}, datatypes::DataType};
    let ValueRef::Union(column, idx) = v else {
        return None;
    };
    let union = column.as_any().downcast_ref::<UnionArray>()?;
    let DataType::Union(fields, _) = column.data_type() else {
        return None;
    };
    let type_id = union.type_id(*idx);
    fields.iter().find(|(id, _)| *id == type_id).map(|(_, field)| field.name().clone())
}

/// A UNION goes out as {"tag": member, "value": ...}; everything else is just
/// its value.
fn emit_tagged(out: &mut String, tag: Option<String>, v: &Value, ty: Option<&LogicalTypeHandle>) {
    match (tag, v) {
        (Some(name), Value::Union(inner)) => {
            let member = ty.and_then(|t| {
                (0..t.num_children()).find(|i| t.child_name(*i) == name).map(|i| t.child(i))
            });
            out.push_str(r#"{"tag":"#);
            push_json_string(out, &name);
            out.push_str(r#","value":"#);
            emit_value(out, inner, member.as_ref());
            out.push('}');
        }
        (_, value) => emit_value(out, value, ty),
    }
}

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
            if *i <= JSON_SAFE as u128 {
                out.push_str(&i.to_string());
            } else {
                push_json_string(out, &i.to_string());
            }
        }
        Value::Float(f) => push_float(out, *f as f64),
        Value::Double(f) => push_float(out, *f),
        Value::Decimal(d) => push_json_string(out, &d.to_string()),
        Value::Text(s) | Value::Enum(s) => push_json_string(out, s),
        Value::Blob(b) if id == Some(LogicalTypeId::Bit) => push_json_string(out, &bit_string(b)),
        // Same JSON-safe rule as every other integer: bare when a double holds
        // it exactly, quoted past that. A BIGNUM is arbitrary precision, so it
        // is usually quoted — but a small one should not look different from
        // the same value in a BIGINT column.
        Value::Blob(b) if id == Some(LogicalTypeId::Bignum) => match varint_to_decimal(b) {
            Some(digits) => match digits.parse::<i128>() {
                Ok(v) => push_int(out, v),
                Err(_) => push_json_string(out, &digits),
            },
            None => push_json_string(out, &base64(b)),
        },
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
    // unsigned_abs, not abs: `i128::MIN` has no positive counterpart, so
    // `abs()` overflows there. In release that wraps back to `i128::MIN`, which
    // compares under the threshold, and the HUGEINT minimum went out as a bare
    // JSON number — the exact silent-reprecision failure this function exists
    // to prevent, on a value any `SELECT (-170141183460469231731687303715884105728)::HUGEINT`
    // produces. In debug it panicked instead.
    if i.unsigned_abs() <= JSON_SAFE as u128 {
        out.push_str(&i.to_string());
    } else {
        push_json_string(out, &i.to_string());
    }
}

fn push_float(out: &mut String, f: f64) {
    // JSON has no NaN or Infinity, but null is not the answer: it is
    // indistinguishable from SQL NULL, so a client cannot tell a missing value
    // from a division that overflowed. The names go out as strings instead.
    if f.is_nan() {
        return push_json_string(out, "NaN");
    }
    if f.is_infinite() {
        return push_json_string(out, if f > 0.0 { "Infinity" } else { "-Infinity" });
    }
    // Rust's Display never switches to exponent notation for large magnitudes,
    // so f64::MAX would go out as 309 digits. Switch at 1e21, which is where
    // JavaScript's own number formatting switches, so the text a client reads
    // is the text it would have produced itself.
    if f != 0.0 && f.abs() >= 1e21 {
        let formatted = format!("{f:e}");
        match formatted.split_once('e') {
            Some((mantissa, exponent)) if !exponent.starts_with('-') => {
                out.push_str(mantissa);
                out.push_str("e+");
                out.push_str(exponent);
            }
            _ => out.push_str(&formatted),
        }
    } else {
        out.push_str(&f.to_string());
    }
}

fn push_json_string(out: &mut String, s: &str) {
    // serde_json owns the escaping rules, including the ones that are easy to
    // get wrong (control characters, lone surrogates).
    let encoded = match serde_json::to_string(s) {
        Ok(encoded) => encoded,
        Err(_) => return out.push_str("\"\""),
    };
    // One rule serde_json correctly does not apply, because it is about the
    // container rather than the value: U+2028 LINE SEPARATOR and U+2029
    // PARAGRAPH SEPARATOR are legal inside a JSON string, but this is a
    // newline-delimited format and they are line terminators to every
    // Unicode-aware line splitter. Left raw, one row is read as two — and the
    // half that is left over is not valid JSON, so a client sees a parse error
    // whose cause is nowhere near where it happened. Escaping them costs a
    // scan that almost always finds nothing.
    if !encoded.contains('\u{2028}') && !encoded.contains('\u{2029}') {
        return out.push_str(&encoded);
    }
    for ch in encoded.chars() {
        match ch {
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            other => out.push(other),
        }
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
    // Six digits for microsecond precision, nine when the value actually
    // carries nanoseconds. Trailing zeros come off either way: a TIMESTAMP_MS
    // of .123 should read as .123, not .123000.
    let mut digits = if nanos % 1_000 == 0 {
        format!("{:06}", nanos / 1_000)
    } else {
        format!("{nanos:09}")
    };
    while digits.ends_with('0') {
        digits.pop();
    }
    out.push('.');
    out.push_str(&digits);
}

/// DuckDB stores BIGNUM (formerly VARINT) as a three-byte header followed by
/// the magnitude, most significant byte first. Without this the value goes out
/// base64-encoded — DuckDB's private storage layout, leaked onto the wire,
/// where no client could read it and nothing would say it was wrong.
///
/// The header's top bit is the sign: 1 positive, 0 negative. Its remaining 23
/// bits are the number of magnitude bytes. For negative values *both* the
/// length field and the magnitude are stored one's-complemented, which is what
/// makes the raw bytes sort correctly as unsigned — and what makes a decoder
/// that only complements the magnitude quietly wrong about the length.
///
/// Returns `None` if the bytes are not a well-formed BIGNUM, so the caller can
/// fall back rather than emit a confidently wrong number.
fn varint_to_decimal(bytes: &[u8]) -> Option<String> {
    const HEADER: usize = 3;
    if bytes.len() < HEADER {
        return None;
    }
    let positive = bytes[0] & 0x80 != 0;
    let raw = (u32::from(bytes[0]) << 16) | (u32::from(bytes[1]) << 8) | u32::from(bytes[2]);
    let declared = if positive { raw & 0x7f_ffff } else { !raw & 0x7f_ffff };
    let data = &bytes[HEADER..];
    if declared as usize != data.len() {
        return None;
    }

    let mut magnitude: Vec<u8> =
        if positive { data.to_vec() } else { data.iter().map(|b| !b).collect() };

    // Long division by 10^9, most significant byte first, taking nine decimal
    // digits per pass. Each quotient digit is `(rem << 8 | byte) / 10^9` with
    // `rem < 10^9`, so it is always under 256 and a byte can hold it.
    let first = magnitude.iter().position(|&b| b != 0).unwrap_or(magnitude.len());
    magnitude.drain(..first);
    if magnitude.is_empty() {
        return Some("0".into());
    }
    let mut groups: Vec<u32> = Vec::new();
    while !magnitude.is_empty() {
        let mut rem: u64 = 0;
        let mut quotient: Vec<u8> = Vec::with_capacity(magnitude.len());
        for &b in &magnitude {
            let cur = (rem << 8) | u64::from(b);
            quotient.push((cur / 1_000_000_000) as u8);
            rem = cur % 1_000_000_000;
        }
        groups.push(rem as u32);
        let nz = quotient.iter().position(|&b| b != 0).unwrap_or(quotient.len());
        magnitude = quotient[nz..].to_vec();
    }

    let mut out = String::with_capacity(groups.len() * 9 + 1);
    if !positive {
        out.push('-');
    }
    // The most significant group carries no leading zeros; every later one is
    // padded to the full nine digits it was divided out as.
    out.push_str(&groups.pop().unwrap_or(0).to_string());
    while let Some(g) = groups.pop() {
        out.push_str(&format!("{g:09}"));
    }
    Some(out)
}

/// DuckDB stores BIT as a leading pad-count byte followed by the bits, most
/// significant first. Without this a bit string goes out base64-encoded, which
/// is not wrong so much as unusable.
fn bit_string(bytes: &[u8]) -> String {
    let Some((&padding, data)) = bytes.split_first() else {
        return String::new();
    };
    let skip = padding as usize;
    let mut out = String::with_capacity(data.len() * 8);
    for (i, byte) in data.iter().enumerate() {
        for bit in (0..8).rev() {
            if i * 8 + (7 - bit) >= skip {
                out.push(if byte >> bit & 1 == 1 { '1' } else { '0' });
            }
        }
    }
    out
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
fn random_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ==========================================================================
//
// DuckDB's reserved words, generated
//
// ==========================================================================


// Generated from `SELECT keyword_name FROM duckdb_keywords()` on DuckDB v1.5.5.
//
// DuckDB quotes an identifier in a type string when it is any keyword at all,
// reserved or not — which is why a STRUCT field called `name` comes back as
// STRUCT("name" VARCHAR). Reproducing that keeps the duckdbType string valid
// SQL, so a client can paste it back into a CREATE TABLE.

/// Sorted, lowercase. Binary-searched, so it must stay sorted.

static KEYWORDS: &[&str] = &[
    "abort", "absolute", "access", "action", "add", "admin", "after", "aggregate", "all",
    "also", "alter", "always", "analyse", "analyze", "and", "anti", "any", "array", "as",
    "asc", "asof", "assertion", "assignment", "asymmetric", "at", "attach", "attribute",
    "authorization", "backward", "before", "begin", "between", "bigint", "binary", "bit",
    "boolean", "both", "by", "cache", "call", "called", "cascade", "cascaded", "case", "cast",
    "catalog", "centuries", "century", "chain", "char", "character", "characteristics",
    "check", "checkpoint", "class", "close", "cluster", "coalesce", "collate", "collation",
    "column", "columns", "comment", "comments", "commit", "committed", "compression",
    "concurrently", "configuration", "conflict", "connection", "constraint", "constraints",
    "content", "continue", "conversion", "copy", "cost", "create", "cross", "csv", "cube",
    "current", "cursor", "cycle", "data", "database", "day", "days", "deallocate", "dec",
    "decade", "decades", "decimal", "declare", "default", "defaults", "deferrable", "deferred",
    "definer", "delete", "delimiter", "delimiters", "depends", "desc", "describe", "detach",
    "dictionary", "disable", "discard", "distinct", "do", "document", "domain", "double",
    "drop", "each", "else", "enable", "encoding", "encrypted", "end", "enum", "error",
    "escape", "event", "except", "exclude", "excluding", "exclusive", "execute", "exists",
    "explain", "export", "export_state", "extension", "extensions", "external", "extract",
    "false", "family", "fetch", "filter", "first", "float", "following", "for", "force",
    "foreign", "forward", "freeze", "from", "full", "function", "functions", "generated",
    "glob", "global", "grant", "granted", "group", "grouping", "grouping_id", "groups",
    "handler", "having", "header", "hold", "hour", "hours", "identity", "if", "ignore",
    "ilike", "immediate", "immutable", "implicit", "import", "in", "include", "including",
    "increment", "index", "indexes", "inherit", "inherits", "initially", "inline", "inner",
    "inout", "input", "insensitive", "insert", "install", "instead", "int", "integer",
    "intersect", "interval", "into", "invoker", "is", "isnull", "isolation", "join", "json",
    "key", "label", "lambda", "language", "large", "last", "lateral", "leading", "leakproof",
    "left", "level", "like", "limit", "listen", "load", "local", "location", "lock", "locked",
    "logged", "macro", "map", "mapping", "match", "matched", "materialized", "maxvalue",
    "merge", "method", "microsecond", "microseconds", "millennia", "millennium", "millisecond",
    "milliseconds", "minute", "minutes", "minvalue", "mode", "month", "months", "move", "name",
    "names", "national", "natural", "nchar", "new", "next", "no", "none", "not", "nothing",
    "notify", "notnull", "nowait", "null", "nullif", "nulls", "numeric", "object", "of", "off",
    "offset", "oids", "old", "on", "only", "operator", "option", "options", "or", "order",
    "ordinality", "others", "out", "outer", "over", "overlaps", "overlay", "overriding",
    "owned", "owner", "parallel", "parser", "partial", "partition", "partitioned", "passing",
    "password", "percent", "persistent", "pivot", "pivot_longer", "pivot_wider", "placing",
    "plans", "policy", "position", "positional", "pragma", "preceding", "precision", "prepare",
    "prepared", "preserve", "primary", "prior", "privileges", "procedural", "procedure",
    "program", "publication", "qualify", "quarter", "quarters", "quote", "range", "read",
    "real", "reassign", "recheck", "recursive", "ref", "references", "referencing", "refresh",
    "reindex", "relative", "release", "rename", "repeatable", "replace", "replica", "reset",
    "respect", "restart", "restrict", "returning", "returns", "revoke", "right", "role",
    "rollback", "rollup", "row", "rows", "rule", "sample", "savepoint", "schema", "schemas",
    "scope", "scroll", "search", "second", "seconds", "secret", "security", "select", "semi",
    "sequence", "sequences", "serializable", "server", "session", "set", "setof", "sets",
    "share", "show", "similar", "simple", "skip", "smallint", "snapshot", "some", "sorted",
    "source", "sql", "stable", "standalone", "start", "statement", "statistics", "stdin",
    "stdout", "storage", "stored", "strict", "strip", "struct", "subscription", "substring",
    "summarize", "symmetric", "sysid", "system", "table", "tables", "tablesample",
    "tablespace", "target", "temp", "template", "temporary", "text", "then", "ties", "time",
    "timestamp", "to", "trailing", "transaction", "transform", "treat", "trigger", "trim",
    "true", "truncate", "trusted", "try_cast", "type", "types", "unbounded", "uncommitted",
    "unencrypted", "union", "unique", "unknown", "unlisten", "unlogged", "unpack", "unpivot",
    "until", "update", "use", "user", "using", "vacuum", "valid", "validate", "validator",
    "value", "values", "varchar", "variable", "variadic", "varying", "verbose", "version",
    "view", "views", "virtual", "volatile", "week", "weeks", "when", "where", "whitespace",
    "window", "with", "within", "without", "work", "wrapper", "write", "xml", "xmlattributes",
    "xmlconcat", "xmlelement", "xmlexists", "xmlforest", "xmlnamespaces", "xmlparse", "xmlpi",
    "xmlroot", "xmlserialize", "xmltable", "year", "years", "yes", "zone",
];

#[cfg(test)]
mod tests {
    use super::civil_from_days;
    use super::ensure_single_statement as one;
    use super::varint_to_decimal;

    /// `civil_from_days` backs both DATE formatting and the log timestamp, and
    /// it was never covered. Pin it to dates whose answers are known
    /// independently: the epoch, both sides of a leap day, the 1900/2000
    /// century rules, and dates before the epoch, where the sign correction on
    /// the era division matters and a plain truncating divide is a day out.
    #[test]
    fn converts_days_to_civil_dates() {
        for (days, want) in [
            (0_i64, (1970_i64, 1_u32, 1_u32)),
            (59, (1970, 3, 1)),      // 1970 is not a leap year
            (-1, (1969, 12, 31)),    // before the epoch
            (-719_468, (0, 3, 1)),   // start of the era
            (11_016, (2000, 2, 29)), // 2000 is a leap year: the /400 rule
            (11_017, (2000, 3, 1)),
            (-25_508, (1900, 3, 1)), // 1900 is not: the /100 rule
            (20_677, (2026, 8, 12)),
            (2_932_896, (9999, 12, 31)),
        ] {
            assert_eq!(civil_from_days(days), want, "days={days}");
        }
    }

    #[test]
    fn accepts_a_single_statement() {
        for sql in [
            "SELECT 1",
            "SELECT 1;",
            "SELECT 1;   \n  ",
            "SELECT ';' AS semi",
            "SELECT 'it''s; fine'",
            "SELECT E'a\\'; b'",
            "SELECT e'a\\'; b'",
            "SELECT (E'a\\'; b')",
            r#"SELECT 1 AS "a;b""#,
            "SELECT $$a; b$$",
            "SELECT $tag$a; b$tag$",
            "SELECT 1 -- trailing; comment",
            "/* a; b */ SELECT 1",
            "/* a /* nested; */ b */ SELECT 1",
            // A keyword ending in `e` butted against a literal. The backslash
            // is data here, not an escape, and the literal ends at the next
            // quote — so there is no second statement and nothing to reject.
            r"SELECT 1 WHERE 'a' LIKE'\'",
        ] {
            assert!(one(sql).is_ok(), "should accept: {sql}");
        }
    }

    /// Every case here was accepted by the scanner before the `e` in `E'...'`
    /// was required to be a token of its own, and each one reached DuckDB as
    /// more than one statement. `duckdb-rs` executes all but the last during
    /// `prepare`, so accepting these dropped the table.
    #[test]
    fn rejects_a_keyword_ending_in_e_used_as_an_escape_string() {
        for sql in [
            r"SELECT 1 WHERE 'a' LIKE'\'; DROP TABLE orders; SELECT 1",
            r"SELECT 1 WHERE 'a' ILIKE'\'; DROP TABLE orders",
            r"SELECT 'a' LIKE 'b' ESCAPE'\'; DROP TABLE orders",
            r"SELECT date'2020-01-01'; DROP TABLE orders",
            r"SELECT time'12:00:00'; DROP TABLE orders",
            // `$1` is a bind parameter, not a dollar-quote tag, so the
            // terminator between these markers is a real one.
            "SELECT $1$; DROP TABLE orders; $1$",
        ] {
            assert!(one(sql).is_err(), "should reject: {sql}");
        }
    }

    /// The tail after a terminator is checked once. When it was re-checked on
    /// every following byte the cost was Θ(n²) — measured at 80 seconds for
    /// 800 KB, so the 8 MiB a request may carry ran for hours on the worker
    /// thread that read it. This finishes instantly or not at all.
    #[test]
    fn scans_a_large_trailing_tail_in_linear_time() {
        let padded = format!("SELECT 1;{}", " ".repeat(4 << 20));
        assert!(one(&padded).is_ok());
        let mut trailing = format!("SELECT 1;{}", " ".repeat(4 << 20));
        trailing.push('x');
        assert!(one(&trailing).is_err());
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

    /// The byte strings here are what DuckDB v1.5.5 actually put on the wire
    /// for these values, captured from a running server rather than derived
    /// from the format description — a decoder tested only against its own
    /// author's reading of the spec proves nothing about the encoder.
    #[test]
    fn decodes_bignum_wire_format() {
        fn hex(s: &str) -> Vec<u8> {
            (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
        }
        for (bytes, want) in [
            ("80000100", "0"),
            ("80000101", "1"),
            ("7ffffefe", "-1"),
            ("8000017f", "127"),
            ("7ffffe80", "-127"),
            ("800001ff", "255"),
            ("7ffffe00", "-255"),
            ("80000d018ee90ff6c373e0ee4e3f0ad2", "123456789012345678901234567890"),
            ("7ffff2fe7116f0093c8c1f11b1c0f52d", "-123456789012345678901234567890"),
        ] {
            assert_eq!(varint_to_decimal(&hex(bytes)).as_deref(), Some(want), "for {bytes}");
        }
    }

    /// Malformed input must return None so the caller can fall back, rather
    /// than produce a confidently wrong number from garbage.
    #[test]
    fn rejects_malformed_bignum() {
        // Too short to hold a header at all.
        assert_eq!(varint_to_decimal(&[]), None);
        assert_eq!(varint_to_decimal(&[0x80, 0x00]), None);
        // Header claims four magnitude bytes; only one follows.
        assert_eq!(varint_to_decimal(&[0x80, 0x00, 0x04, 0x01]), None);
    }
}
