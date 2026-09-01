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
//! steps the result). It holds the raw connection behind an AtomicPtr that
//! `Conn::drop` nulls, so a late canceller aims at nothing rather than at
//! freed memory.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, Ordering};

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
    /// The cross-thread cancellation slot; nulled by drop.
    interrupt: Arc<AtomicPtr<ffi::_connection>>,
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
            interrupt: Arc::new(AtomicPtr::new(conn)),
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
        Ok(Stream { eng: self.eng, result, columns })
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
        // Cancellers see null from here on; the tiny window between a load
        // and this store is the same one v1's InterruptHandle carried.
        self.interrupt.store(std::ptr::null_mut(), Ordering::SeqCst);
        self.cache.clear();
        unsafe {
            if let Some(f) = self.eng.api.disconnect {
                f(&mut self.conn);
            }
        }
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

/// A live result: the connection's cursor. Destroy (drop) promptly — while it
/// lives, the connection refuses new statements.
pub struct Stream {
    eng: &'static Engine,
    result: ffi::result_handle,
    /// The result's columns. Public so a caller can `mem::take` them and
    /// keep them across the mutable borrows the chunk loop needs.
    pub columns: Vec<(String, Type)>,
}

impl Stream {
    /// The next chunk, or None at end-of-stream. An interrupted query
    /// surfaces here as ERROR_RUNTIME_INTERRUPT.
    pub fn next_chunk(&mut self) -> Result<Option<Chunk>, Error> {
        let api = &self.eng.api;
        let mut chunk: ffi::data_chunk_handle = std::ptr::null_mut();
        call!(api, result_fetch_chunk(self.result, &mut chunk));
        if chunk.is_null() {
            return Ok(None);
        }
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

impl Drop for Stream {
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
    conn: Arc<AtomicPtr<ffi::_connection>>,
}

// The spec commits connection_interrupt to any-thread use.
unsafe impl Send for Interrupt {}
unsafe impl Sync for Interrupt {}

impl Interrupt {
    /// Interrupt whatever runs on the connection right now. A no-op when
    /// nothing does, or when the connection is already gone.
    pub fn interrupt(&self) {
        let conn = self.conn.load(Ordering::SeqCst);
        if conn.is_null() {
            return;
        }
        if let Some(f) = self.eng.api.connection_interrupt {
            let mut err: ffi::error_info_handle = std::ptr::null_mut();
            unsafe { f(conn, &mut err) };
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
