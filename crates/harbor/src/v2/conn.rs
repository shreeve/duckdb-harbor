//! The v2 connection: what the server needs from an engine, first-party.
//!
//! The successor to duckdb-rs's `Connection` in harbor's pool. One `Db` is
//! opened per process and shared; each `Conn` is its own engine connection —
//! Send but deliberately not Sync, one executor thread each. Statements are
//! cached parsed-only (a v2 statement is raw parser output; binding happens
//! inside statement_execute), so the cache mitigates the v2 parser's cost
//! without ever holding a stale plan: catalog changes are seen because every
//! execution re-binds.
//!
//! Cancellation: `Interrupt` is a cross-thread handle (the spec commits
//! connection_interrupt to being callable from any thread while another
//! steps the result). It shares a mutex-guarded slot with its Conn: a
//! canceller fires while holding the lock, and `Conn::drop` disconnects and
//! nulls the slot under the same lock — so an interrupt can never land on a
//! freed handle, and a late canceller aims at nothing.

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use super::encode::{Type, result_columns};
use super::{Engine, Error, ffi};

/// One fallible v2 call inside a `Result<_, Error>` function.
macro_rules! call {
    ($api:expr, $f:ident($($a:expr),*)) => {{
        let api = $api;
        let f = api.$f.ok_or_else(|| Error {
            code: ffi::ERROR_API,
            message: concat!("engine lacks duckdb_v2_", stringify!($f)).to_string(),
        })?;
        let mut err: ffi::error_info_handle = std::ptr::null_mut();
        #[allow(unused_unsafe)]
        let code = unsafe { f($($a,)* &mut err) };
        if code != ffi::ERROR_NONE {
            return Err(Error::take(api, code, err));
        }
    }};
}

fn str_of(s: &str) -> ffi::str_t {
    ffi::str_t { ptr: s.as_ptr() as *const _, len: s.len() as ffi::idx_t }
}

fn ident_of(s: &str) -> ffi::identifier_t {
    str_of(s)
}

// ---------------------------------------------------------------------------
// Database: environment + instance, shared by every connection in the pool.
// ---------------------------------------------------------------------------

pub struct Db {
    eng: &'static Engine,
    env: ffi::environment_handle,
    db: ffi::database_handle,
}

// The handles are only used to spawn connections (boot, one thread) and to
// tear down (last drop); the engine guards its own instance internally.
unsafe impl Send for Db {}
unsafe impl Sync for Db {}

impl Drop for Db {
    fn drop(&mut self) {
        unsafe {
            if let Some(f) = self.eng.api.close {
                f(&mut self.db);
            }
            if let Some(f) = self.eng.api.destroy_environment {
                f(&mut self.env);
            }
        }
    }
}

/// Open a database and the first connection on it. `options` are open-time
/// config (name, setting) pairs — the ones a later SET cannot reach.
pub fn open(path: &Path, options: &[(&str, &str)]) -> Result<Conn, Error> {
    let eng = super::engine().map_err(|message| Error { code: ffi::ERROR_API, message })?;
    let api = &eng.api;

    let mut env: ffi::environment_handle = std::ptr::null_mut();
    call!(api, create_environment(&mut env));

    // Options are handles; build them, open, destroy them either way.
    let mut opts: Vec<ffi::option_handle> = Vec::with_capacity(options.len());
    let mut build = || -> Result<(), Error> {
        for (name, setting) in options {
            let mut o: ffi::option_handle = std::ptr::null_mut();
            call!(api, option_create(ident_of(name), str_of(setting), &mut o));
            opts.push(o);
        }
        Ok(())
    };
    let built = build();

    let path_text = path.to_string_lossy();
    let mut db: ffi::database_handle = std::ptr::null_mut();
    let opened = match built {
        Ok(()) => (|| -> Result<(), Error> {
            call!(
                api,
                open(env, str_of(&path_text), opts.as_mut_ptr(), opts.len() as ffi::idx_t, &mut db)
            );
            Ok(())
        })(),
        Err(e) => Err(e),
    };
    for mut o in opts {
        if let Some(f) = api.option_destroy {
            unsafe { f(&mut o) };
        }
    }
    if let Err(e) = opened {
        if let Some(f) = api.destroy_environment {
            unsafe { f(&mut env) };
        }
        return Err(e);
    }

    let shared = Arc::new(Db { eng, env, db });
    Conn::connect(shared)
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

/// How many distinct statement texts each connection keeps parsed. Matches
/// the v1 prepared-statement cache the executor relied on.
const STMT_CACHE_CAP: usize = 64;

pub struct Conn {
    eng: &'static Engine,
    db: Arc<Db>,
    conn: ffi::connection_handle,
    /// The cross-thread cancellation slot. A canceller holds this lock
    /// across its connection_interrupt call, and drop holds it across
    /// disconnect — so an interrupt can never land on a freed handle.
    interrupt: Arc<Mutex<ffi::connection_handle>>,
    /// Parsed-statement cache: SQL text → statements, LRU by tick.
    cache: HashMap<String, CacheEntry>,
    tick: u64,
}

unsafe impl Send for Conn {}

struct CacheEntry {
    /// Arc, so a caller can hold the statements across its own `execute`
    /// calls without pinning the cache map's borrow — and so eviction under
    /// a live run frees nothing early.
    stmts: Arc<Vec<Stmt>>,
    used: u64,
}

impl Conn {
    fn connect(db: Arc<Db>) -> Result<Conn, Error> {
        let eng = db.eng;
        let api = &eng.api;
        let mut conn: ffi::connection_handle = std::ptr::null_mut();
        call!(api, connect(db.db, &mut conn));
        Ok(Conn {
            eng,
            db,
            conn,
            interrupt: Arc::new(Mutex::new(conn)),
            cache: HashMap::new(),
            tick: 0,
        })
    }

    /// Another connection to the same database — the pool's clone.
    pub fn try_clone(&self) -> Result<Conn, Error> {
        Conn::connect(self.db.clone())
    }

    pub fn engine_version(&self) -> &'static str {
        &self.eng.version
    }

    /// The raw function table, for the encoders.
    pub fn api(&self) -> &'static ffi::Api {
        &self.eng.api
    }

    /// Run one statement and return its first column as text, row per entry —
    /// the boot-time helper behind `SELECT version()` and the catalog list.
    pub fn query_strings(&mut self, sql: &str) -> Result<Vec<String>, Error> {
        let api = &self.eng.api;
        let stmts = self.statements(sql)?;
        let mut out = Vec::new();
        for stmt in stmts.iter() {
            let mut stream = self.execute(stmt, &[])?;
            let columns = std::mem::take(&mut stream.columns);
            while let Some(chunk) = stream.next_chunk()? {
                let readers = chunk.readers(columns.len().min(1))?;
                if let (Some(reader), Some((_, ty))) = (readers.first(), columns.first()) {
                    for row in 0..chunk.rows {
                        let mut cell = String::new();
                        super::encode::emit_cell(&mut cell, api, reader, ty, row)?;
                        // The helper reads VARCHAR columns; strip the JSON
                        // quoting the encoder applies.
                        out.push(
                            serde_json::from_str::<String>(&cell)
                                .unwrap_or_else(|_| cell.trim_matches('"').to_string()),
                        );
                    }
                }
            }
        }
        Ok(out)
    }

    /// The cancellation handle for this connection.
    pub fn interrupt_handle(&self) -> Arc<Interrupt> {
        Arc::new(Interrupt { eng: self.eng, conn: self.interrupt.clone() })
    }

    /// Set one config option (GLOBAL scope on the database).
    pub fn set_option(&self, name: &str, setting: &str) -> Result<(), Error> {
        let api = &self.eng.api;
        let mut o: ffi::option_handle = std::ptr::null_mut();
        call!(api, option_create(ident_of(name), str_of(setting), &mut o));
        let set = (|| -> Result<(), Error> {
            call!(api, database_option_set(self.db.db, o));
            Ok(())
        })();
        if let Some(f) = api.option_destroy {
            let mut o = o;
            unsafe { f(&mut o) };
        }
        set
    }

    /// Parse a SQL string into its statements. Uncached.
    fn parse(&self, sql: &str) -> Result<Vec<Stmt>, Error> {
        let api = &self.eng.api;
        let c = std::ffi::CString::new(sql)
            .map_err(|_| Error { code: ffi::ERROR_INPUT_INVALID, message: "SQL contains a NUL byte".into() })?;
        let mut iter: ffi::statement_iterator_handle = std::ptr::null_mut();
        call!(api, parse_sql(self.conn, c.as_ptr(), &mut iter));
        let mut stmts = Vec::new();
        let walked = (|| -> Result<(), Error> {
            loop {
                let mut stmt: ffi::sql_statement_handle = std::ptr::null_mut();
                call!(api, statement_iterator_next(iter, &mut stmt));
                if stmt.is_null() {
                    return Ok(());
                }
                stmts.push(Stmt { eng: self.eng, raw: stmt });
            }
        })();
        if let Some(f) = api.statement_iterator_destroy {
            let mut iter = iter;
            unsafe { f(&mut iter) };
        }
        walked.map(|_| stmts)
    }

    /// The parsed statements for this SQL, from cache or freshly parsed.
    /// Statements are parse-only, so reuse can never see a stale binding.
    pub fn statements(&mut self, sql: &str) -> Result<Arc<Vec<Stmt>>, Error> {
        self.tick += 1;
        if !self.cache.contains_key(sql) {
            let stmts = Arc::new(self.parse(sql)?);
            if self.cache.len() >= STMT_CACHE_CAP {
                if let Some(oldest) = self
                    .cache
                    .iter()
                    .min_by_key(|(_, e)| e.used)
                    .map(|(k, _)| k.clone())
                {
                    self.cache.remove(&oldest);
                }
            }
            self.cache.insert(sql.to_string(), CacheEntry { stmts, used: self.tick });
        }
        let entry = self.cache.get_mut(sql).expect("just inserted");
        entry.used = self.tick;
        Ok(entry.stmts.clone())
    }

    /// Execute one parsed statement. The statement is borrowed, not consumed.
    pub fn execute(&self, stmt: &Stmt, params: &[Param]) -> Result<Stream, Error> {
        let api = &self.eng.api;

        // Params go in as owned values, positional ($1 = element 0).
        let mut values: Vec<ffi::value_handle> = Vec::with_capacity(params.len());
        let bound = (|| -> Result<(), Error> {
            for p in params {
                values.push(p.to_value(self.eng, self.conn)?);
            }
            Ok(())
        })();

        let mut result: ffi::result_handle = std::ptr::null_mut();
        let executed = match bound {
            Ok(()) => (|| -> Result<(), Error> {
                call!(
                    api,
                    statement_execute(
                        self.conn,
                        stmt.raw,
                        std::ptr::null(),
                        values.as_ptr(),
                        values.len() as ffi::idx_t,
                        &mut result
                    )
                );
                Ok(())
            })(),
            Err(e) => Err(e),
        };
        for mut v in values {
            if let Some(f) = api.value_destroy {
                unsafe { f(&mut v) };
            }
        }
        executed?;

        let columns = match result_columns(api, result) {
            Ok(c) => c,
            Err(e) => {
                if let Some(f) = api.result_destroy {
                    unsafe { f(&mut result) };
                }
                return Err(e);
            }
        };

        // Hand the result to its fetch thread; from here the Fetcher owns
        // it (destroying it on every exit path, including a failed spawn).
        let fetcher = Fetcher { eng: self.eng, result };
        let (tx, rx) = mpsc::sync_channel(PREFETCH);
        let join = thread::Builder::new()
            .name("harbor-fetch".into())
            .spawn(move || fetcher.run(tx))
            .map_err(|e| Error {
                code: ffi::ERROR_API,
                message: format!("could not spawn fetch thread: {e}"),
            })?;
        Ok(Stream {
            columns,
            rx: Some(rx),
            join: Some(join),
            interrupt: Interrupt { eng: self.eng, conn: self.interrupt.clone() },
            done: false,
        })
    }

    /// Parse and run a whole SQL string, draining every result. The
    /// counterpart of duckdb-rs execute_batch: SETs, ROLLBACK, CHECKPOINT.
    pub fn execute_batch(&mut self, sql: &str) -> Result<(), Error> {
        for stmt in self.parse(sql)? {
            let mut stream = self.execute(&stmt, &[])?;
            while stream.next_chunk()?.is_some() {}
        }
        Ok(())
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.cache.clear();
        // Disconnect under the interrupt lock: a canceller mid-call finishes
        // against the live handle first, and every later one sees null.
        let mut slot = self.interrupt.lock().unwrap_or_else(|p| p.into_inner());
        unsafe {
            if let Some(f) = self.eng.api.disconnect {
                f(&mut self.conn);
            }
        }
        *slot = std::ptr::null_mut();
    }
}

/// An owned parsed statement.
pub struct Stmt {
    eng: &'static Engine,
    raw: ffi::sql_statement_handle,
}

// Immutable after parse; statement_execute borrows and runs a copy. The
// handle crosses threads only inside its owning Conn.
unsafe impl Send for Stmt {}
unsafe impl Sync for Stmt {}

impl Drop for Stmt {
    fn drop(&mut self) {
        unsafe {
            if let Some(f) = self.eng.api.sql_statement_destroy {
                f(&mut self.raw);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Streaming result
// ---------------------------------------------------------------------------

/// Chunks buffered between the fetch thread and the consumer. Enough that
/// the fetch thread can run ahead while the consumer encodes, small enough
/// to bound memory: a chunk is at most 2048 rows.
const PREFETCH: usize = 4;

/// A live result, drained by a dedicated fetch thread and consumed here as
/// a channel of chunks. Drop promptly — while the result lives, the
/// connection refuses new statements.
///
/// The pipeline is the point: fetching and encoding used to share one
/// thread, so every fetch stall — above all the engine's 20ms WaitForTask
/// nap between chunks — sat on the critical path, and every encode ran
/// with the engine idle. With a fetch thread, the engine produces chunk
/// N+1 (on its full worker pool) while the consumer encodes chunk N; a
/// nap only costs wall time when the consumer has nothing left to chew.
pub struct Stream {
    /// The result's columns. Public so a caller can `mem::take` them and
    /// keep them across the mutable borrows the chunk loop needs.
    pub columns: Vec<(String, Type)>,
    /// Taken (closed) on drop, so the fetch thread's next send fails and
    /// it stops fetching.
    rx: Option<mpsc::Receiver<Result<Chunk, Error>>>,
    join: Option<thread::JoinHandle<()>>,
    /// Fired on drop when the fetch thread is still mid-query: an
    /// abandoned stream must not leave the fetch thread blocked inside the
    /// engine, because the result it holds keeps the connection refusing
    /// statements. The engine clears the flag at the next query's start
    /// (ClientContext::InitialCleanup), so a stray interrupt cannot poison
    /// the statement after this one.
    interrupt: Interrupt,
    done: bool,
}

impl Stream {
    /// The next chunk, or None at end-of-stream. An interrupted query
    /// surfaces here as ERROR_RUNTIME_INTERRUPT.
    pub fn next_chunk(&mut self) -> Result<Option<Chunk>, Error> {
        if self.done {
            return Ok(None);
        }
        let Some(rx) = self.rx.as_ref() else {
            return Ok(None);
        };
        match rx.recv() {
            Ok(Ok(chunk)) => Ok(Some(chunk)),
            Ok(Err(e)) => {
                self.done = true;
                Err(e)
            }
            // Sender gone with no error sent: the stream ended.
            Err(mpsc::RecvError) => {
                self.done = true;
                Ok(None)
            }
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // Close the channel first: the fetch thread's next send fails and
        // it destroys the result. If it is still inside the engine —
        // blocked in a fetch, or mid-computation — interrupt so it comes
        // back promptly; a fully drained stream's thread has already
        // exited and the join is instant.
        self.rx.take();
        if let Some(join) = self.join.take() {
            if !join.is_finished() {
                self.interrupt.interrupt();
            }
            let _ = join.join();
        }
    }
}

/// The fetch thread's half of a Stream: exclusive owner of the result
/// handle. The result is single-consumer by spec — moving it wholesale to
/// one thread is exactly the contract.
struct Fetcher {
    eng: &'static Engine,
    result: ffi::result_handle,
}

// The result handle is owned exclusively by the fetch thread once spawned;
// nothing else touches it.
unsafe impl Send for Fetcher {}

impl Fetcher {
    /// Fetch until end, error, or the consumer hangs up.
    fn run(mut self, tx: mpsc::SyncSender<Result<Chunk, Error>>) {
        loop {
            match self.next_chunk() {
                Ok(Some(chunk)) => {
                    if tx.send(Ok(chunk)).is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = tx.send(Err(e));
                    break;
                }
            }
        }
    }

    /// The next chunk, or None at end-of-stream. An interrupted query
    /// surfaces here as ERROR_RUNTIME_INTERRUPT.
    ///
    /// Driven through result_step, with a time-budgeted fallback to
    /// result_fetch_chunk. The two primitives fail in opposite ways. The
    /// blocking fetch engages the engine's full worker pool but parks in
    /// Executor::WaitForTask whenever it has no runnable task — a bounded
    /// 20ms condition wait signalled on task reschedule, not on
    /// chunk-became-ready — and on a streaming query those naps gate
    /// production. Stepping never naps, and on this dedicated thread the
    /// engine work each step runs overlaps the consumer's encoding on
    /// another core — but a step is one bounded work unit on THIS thread
    /// only, so a compute-heavy pipeline driven entirely by steps executes
    /// serially (measured 17x slower on a wide aggregation).
    ///
    /// Hence the time budget, per chunk: a streaming producer hands back
    /// its chunk in tens of microseconds of stepping and never trips it; a
    /// chunk still unproduced after the budget marks a compute phase, and
    /// the blocking fetch takes over — full parallelism, one nap, and at
    /// most a budget's worth of serial work lost per chunk, on a query
    /// shape with few chunks. The budget is per next_chunk call, so a
    /// heavy phase followed by a streaming tail (a big sort, say) pays it
    /// only while the phase lasts and steps again from the next chunk on.
    /// Counting steps does not work as a budget: a step's work unit is far
    /// smaller than a chunk's production, so any count small enough to
    /// protect heavy queries trips constantly on streaming ones.
    fn next_chunk(&mut self) -> Result<Option<Chunk>, Error> {
        use std::time::{Duration, Instant};
        // Sized to clear a streaming query's most expensive chunk — the
        // first, which carries execution start-up (measured ~2ms on shapes
        // that produce every later chunk in tens of microseconds). Tripping
        // the budget costs a ~20ms nap in the blocking fetch, so a
        // too-tight budget taxes exactly the queries it exists to protect.
        const STEP_BUDGET: Duration = Duration::from_millis(5);
        // How many steps between clock checks; a step is well under a
        // microsecond of overhead, the clock read is not free.
        const CLOCK_EVERY: u32 = 64;
        let api = &self.eng.api;
        let Some(step) = api.result_step else {
            return self.next_chunk_blocking();
        };
        let start = Instant::now();
        let mut n = 0u32;
        loop {
            let mut chunk: ffi::data_chunk_handle = std::ptr::null_mut();
            let mut status: ffi::RESULT_STEP_STATUS = ffi::RESULT_STEP_STATUS_WAITING;
            let mut err: ffi::error_info_handle = std::ptr::null_mut();
            let code = unsafe { step(self.result, &mut chunk, &mut status, &mut err) };
            if code != ffi::ERROR_NONE {
                return Err(Error::take(api, code, err));
            }
            match status {
                ffi::RESULT_STEP_STATUS_CHUNK => return self.sized(chunk),
                ffi::RESULT_STEP_STATUS_FINISHED => return Ok(None),
                // The same shape result_fetch_chunk reports for a cancelled
                // query: the code the callers key on, with the engine's
                // rendering of the InterruptException it would have thrown.
                ffi::RESULT_STEP_STATUS_CANCELLED => {
                    return Err(Error {
                        code: ffi::ERROR_RUNTIME_INTERRUPT,
                        message: "INTERRUPT Error: Interrupted!".to_string(),
                    });
                }
                _ => std::thread::yield_now(),
            }
            n += 1;
            if n % CLOCK_EVERY == 0 && start.elapsed() >= STEP_BUDGET {
                return self.next_chunk_blocking();
            }
        }
    }

    /// The engine's own blocking fetch: full worker-pool parallelism, at
    /// the cost of the WaitForTask nap. The heavy-shape half of
    /// next_chunk, and the whole path for a build without result_step.
    fn next_chunk_blocking(&mut self) -> Result<Option<Chunk>, Error> {
        let api = &self.eng.api;
        let mut chunk: ffi::data_chunk_handle = std::ptr::null_mut();
        call!(api, result_fetch_chunk(self.result, &mut chunk));
        if chunk.is_null() {
            return Ok(None);
        }
        self.sized(chunk)
    }

    /// Wrap a non-null chunk handle with its row count.
    fn sized(&self, mut chunk: ffi::data_chunk_handle) -> Result<Option<Chunk>, Error> {
        let api = &self.eng.api;
        let mut rows: ffi::idx_t = 0;
        let sized = (|| -> Result<(), Error> {
            call!(api, data_chunk_get_size(chunk, &mut rows));
            Ok(())
        })();
        match sized {
            Ok(()) => Ok(Some(Chunk { eng: self.eng, raw: chunk, rows: rows as usize })),
            Err(e) => {
                if let Some(f) = api.data_chunk_destroy {
                    unsafe { f(&mut chunk) };
                }
                Err(e)
            }
        }
    }
}

impl Drop for Fetcher {
    fn drop(&mut self) {
        unsafe {
            if let Some(f) = self.eng.api.result_destroy {
                f(&mut self.result);
            }
        }
    }
}

/// One caller-owned chunk of rows.
pub struct Chunk {
    eng: &'static Engine,
    raw: ffi::data_chunk_handle,
    pub rows: usize,
}

// Owned exclusively by whoever holds it; the fetch thread produces chunks
// and hands them across the channel to the consumer.
unsafe impl Send for Chunk {}

impl Chunk {
    /// Build the readers for the chunk's columns. Valid until the chunk is
    /// dropped; the borrow ties them to it.
    pub fn readers(&self, count: usize) -> Result<Vec<super::encode::Reader>, Error> {
        let api = &self.eng.api;
        let mut readers = Vec::with_capacity(count);
        for i in 0..count {
            let mut vector: ffi::vector_handle = std::ptr::null_mut();
            call!(api, data_chunk_get_vector(self.raw, i as ffi::idx_t, &mut vector));
            readers.push(super::encode::Reader::of(api, vector)?);
        }
        Ok(readers)
    }
}

impl Drop for Chunk {
    fn drop(&mut self) {
        unsafe {
            if let Some(f) = self.eng.api.data_chunk_destroy {
                f(&mut self.raw);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------

/// Cross-thread cancellation for one connection.
pub struct Interrupt {
    eng: &'static Engine,
    conn: Arc<Mutex<ffi::connection_handle>>,
}

// The spec commits connection_interrupt to any-thread use.
unsafe impl Send for Interrupt {}
unsafe impl Sync for Interrupt {}

impl Interrupt {
    /// Interrupt whatever runs on the connection right now. A no-op when
    /// nothing does, or when the connection is already gone. Held under the
    /// same lock Conn::drop disconnects under, so the handle it fires at is
    /// alive for the duration of the call; connection_interrupt only sets a
    /// flag, so the hold is momentary.
    pub fn interrupt(&self) {
        let slot = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_null() {
            return;
        }
        if let Some(f) = self.eng.api.connection_interrupt {
            let mut err: ffi::error_info_handle = std::ptr::null_mut();
            unsafe { f(*slot, &mut err) };
            if !err.is_null() {
                if let Some(d) = self.eng.api.error_info_destroy {
                    unsafe { d(&mut err) };
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

/// A client-supplied statement parameter — the JSON-reachable values.
#[derive(Clone, Debug)]
pub enum Param {
    Null,
    Bool(bool),
    I64(i64),
    U64(u64),
    F64(f64),
    Text(String),
}

impl Param {
    fn to_value(&self, eng: &'static Engine, conn: ffi::connection_handle) -> Result<ffi::value_handle, Error> {
        let api = &eng.api;
        let mut out: ffi::value_handle = std::ptr::null_mut();
        match self {
            // A NULL needs a type; INTEGER's null casts to anything at bind.
            Param::Null => {
                let mut ty: ffi::logical_type_handle = std::ptr::null_mut();
                call!(
                    api,
                    connection_create_type_from_id(
                        conn,
                        ffi::LOGICAL_TYPE_ID_INTEGER,
                        std::ptr::null(),
                        std::ptr::null(),
                        0,
                        &mut ty
                    )
                );
                let made = (|| -> Result<(), Error> {
                    call!(api, value_create_null_with_connection(conn, ty, &mut out));
                    Ok(())
                })();
                if let Some(f) = api.logical_type_destroy {
                    let mut ty = ty;
                    unsafe { f(&mut ty) };
                }
                made?;
            }
            Param::Bool(b) => call!(api, value_create_bool_with_connection(conn, *b, &mut out)),
            Param::I64(i) => call!(api, value_create_bigint_with_connection(conn, *i, &mut out)),
            Param::U64(u) => call!(api, value_create_ubigint_with_connection(conn, *u, &mut out)),
            Param::F64(f) => call!(api, value_create_double_with_connection(conn, *f, &mut out)),
            Param::Text(s) => {
                call!(api, value_create_varchar_with_connection(conn, str_of(s), &mut out))
            }
        }
        Ok(out)
    }
}
