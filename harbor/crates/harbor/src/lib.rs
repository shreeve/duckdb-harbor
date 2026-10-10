// harbor's library — the server engine: pool, leases, cancellation,
// timeouts, the NDJSON envelope, /sql /catalog /ready routing, and the
// SIGTERM → drain → CHECKPOINT shutdown path. The CLI (src/main.rs) is the
// only consumer — one crate, bin beside lib. The embedding host —
// `harbor start` — opens the DuckDB Connection, hands it to `open_pool`, and
// calls `start`/`wait`/`stop`.

use std::{
    collections::HashMap,

    fmt::Write as _,
    io::Read,
    panic::AssertUnwindSafe,
    sync::{
        Arc, Condvar, Mutex, OnceLock, mpsc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use justhttp::{Header, Method, Request, Response, Server};
use wire::catalog::{Catalog, Column, ForeignKey, Index, Inventory, Named, Sequence, Table, Unique};
use wire::code;
use wire::statement::{acting_keyword, bare_word, commits, transaction_effect};

use crate::engine::conn::{Conn as Connection, Interrupt as InterruptHandle, Param};

mod encode;
use encode::*;

// Brace expansion, applied to every statement before the engine sees it.
mod unbrace;

/// The client half — REPL, renderer, transports. Lives in the same
/// crate so both halves of `harbor` are one codebase with one version;
/// the server half (this file, `encode`, `unbrace`, `engine`) never calls
/// into it. The SQL lexer both halves read with is `wire::scan`.
pub mod repl;

// The v2 C API engine, generated from DuckDB's api_spec: the one path to the
// engine.
pub mod engine;

// ==========================================================================
//
// The HTTP server
//
// ==========================================================================


// The HTTP side of harbor.
//
// Shape (the data plane is deliberately small; the rest is bookkeeping):
//
//   GET    /ready               can this server answer a query?
//   POST   /shutdown            drain, CHECKPOINT, exit (the graceful stop)
//   GET    /info                who am I, serving what, since when
//   GET    /catalog             schema document for completion and browsers
//   POST   /sql                 run one statement, stream the NDJSON envelope
//   POST   /sql/sessions        open a lease (a pinned connection)
//   GET    /sql/sessions        list ALL open leases — holder, age, deadline
//   DELETE /sql/sessions/<id>   release that one
//   DELETE /sql/queries/<id>    cancel a statement the caller named
//
// Also served: POST /sql/sessions/new, the route Rip's driver opens sessions
// at; GET /sessions; DELETE /shutdown.
//
// The envelope is the one thing that must not drift, because it is the
// contract every client speaks:
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
// One statement per request keeps HTTP outcomes unambiguous. Bind parameters
// for values: a single statement can still contain SQL injection if built by
// concatenation. Multi-statement work belongs on a session.
//
// Concurrency: accept many connections, execute few queries. DuckDB
// parallelises a single query across all cores, so running hundreds
// concurrently buys thrashing, not throughput. A fixed worker pool bounds
// in-flight statements; a connection past that waits for a worker to come
// free. It does not wait in the kernel accept backlog — justhttp accepts
// eagerly on its own thread and gives each connection an OS thread, so
// connection count, not worker count, is what a flood actually costs.



/// Bounded number of statements executing at once. Connections may greatly
/// exceed this; queries should not.
pub const DEFAULT_MAX_INFLIGHT: usize = 6;

/// Largest request body we will read, declared or delivered. A statement is
/// text, and a megabyte of it is already pathological — the limit sits well
/// above that so a generous `params` array is never the thing that fails.
const MAX_BODY: usize = 8 << 20;

/// Stack for the threads that run SQL. The engine recurses once per level
/// when it turns a JSON document into a VARIANT, and on the 2 MiB default a
/// document some 7,700 levels deep overflowed the executor and took the
/// whole server down — with every client's connection. Sixteen mebibytes
/// puts that past 60,000 levels, and the pages are reserved, not committed,
/// until a stack actually grows into them, so an idle thread costs nothing
/// extra.
const EXEC_STACK: usize = 16 << 20;

/// Rows are buffered to roughly this size before hitting the socket. Small
/// enough that a slow client sees data promptly, large enough that a wide
/// result is not one syscall per row.
const FLUSH_AT: usize = 64 << 10;

/// The largest one-shot JSON document harbor will build.
///
/// A JSON document is not valid until its last byte, so this shape cannot flush
/// as it goes the way NDJSON does — the whole result is held in memory, once per
/// concurrent request. Without a ceiling, one `SELECT * FROM a_big_table` with
/// the wrong Accept header takes the process down. The number is generous for
/// what one-shot is for (a small result in a single round trip) and the remedy
/// for anything larger is the default: NDJSON streams with no size limit.
const MAX_JSON_RESPONSE: usize = 32 << 20;

// ---------------------------------------------------------------------------
// Process-wide state
//
// harbor IS DuckDB's process, so "the server" is a process singleton: one
// listener, one worker pool.
//
// Pool slots are allocated before the listener starts. Their number stays
// fixed (workers + leases); a slot replaces its engine connection whenever
// connection-local state must be discarded before reuse.
//
// One connection per worker, because a DuckDB connection is Send but not
// Sync — two threads may not share one.
// ---------------------------------------------------------------------------

/// How many connection slots to allocate at load when nothing says otherwise.
///
/// The default covers the default six workers with ten left over for leases.
/// `HARBOR_POOL_SIZE` moves it, and has to, because this is the one number
/// that cannot be changed once the pool is opened: a deployment that
/// wants more concurrent transactions than ten has no other way to ask.
const DEFAULT_POOL_SIZE: usize = 16;
const MIN_POOL_SIZE: usize = 2;
const MAX_POOL_SIZE: usize = 256;

fn configured_pool_size() -> usize {
    match std::env::var("HARBOR_POOL_SIZE").ok().and_then(|v| v.trim().parse::<usize>().ok()) {
        Some(n) => n.clamp(MIN_POOL_SIZE, MAX_POOL_SIZE),
        None => DEFAULT_POOL_SIZE,
    }
}

/// Connections handed out to workers when the server starts, returned when it
/// stops.
static POOL: Mutex<Vec<Connection>> = Mutex::new(Vec::new());

/// Reserved for harbor's own statements — the shutdown CHECKPOINT — so it is
/// never waiting behind a client query.
static CONTROL: Mutex<Option<Connection>> = Mutex::new(None);

/// CONTROL's cancellation slot, taken at load beside the connection itself.
///
/// Without it CONTROL was the one connection nothing could interrupt, and it
/// is on the shutdown path: the probe thread answers `/ready` there while
/// holding CONTROL's mutex, and `stop()` needs that same mutex for the
/// CHECKPOINT. A readiness query that never returned would have held the lock
/// and the shutdown with it, with no way to break the tie. Registered in
/// SLOTS at `start()` like every other executor, so the reaper and the
/// cancel-all in `stop()` reach it by the same path — and by job id, so a
/// cancel can never land on the CHECKPOINT, which runs after SLOTS is empty.
static CONTROL_SLOT: Mutex<Option<Arc<SlotState>>> = Mutex::new(None);

static RUNNING: Mutex<Option<Running>> = Mutex::new(None);

/// Woken when the server stops, so `wait()` can block without polling.
static STOPPED: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());

// ---------------------------------------------------------------------------
// Cancellation
//
// A statement that has entered DuckDB does not come back until it is done.
// Harbor executes a small, bounded number of statements at once, so one query
// that runs forever is not a slow request — it is a worker removed from
// service, and six of them are the whole server. Four things can ask a
// statement to stop:
//
//   1. The client, by naming its own `queryId` and sending DELETE to it, or by
//      releasing a session whose statement is still running.
//   2. The client, by hanging up: the statement it can no longer be told
//      about is stopped, whether or not a row of it has been written.
//   3. A deadline, when one was asked for.
//   4. The reaper, when a lease has outlived its TTL while busy: the lease
//      that most needs reclaiming is the one wedged inside a runaway
//      statement.
//
// `duckdb_interrupt` is per connection, not per database (`InterruptHandle`
// wraps a `duckdb_connection`), so interrupting one statement cannot disturb
// another — including control work running on the dedicated connection,
// which is not in the worker pool at all.
//
// The hazard worth naming is the one that makes this subtle: an interrupt is
// aimed at a connection, but the thing being cancelled is a *statement*, and
// between deciding to cancel and firing, the target can finish and the next
// statement can start on the same connection. Interrupting then kills an
// innocent query, and the symptom — a query that fails once, at random, under
// load — is close to undebuggable. So every statement carries a process-unique
// id, and a cancel is "interrupt job N, if job N is still what is running",
// checked and fired under the same lock the executor must take to retire it.
// ---------------------------------------------------------------------------

/// What one executor is doing, and how to stop it.
struct SlotState {
    interrupt: Arc<InterruptHandle>,
    run: Mutex<SlotRun>,
}

struct SlotRun {
    /// The statement running right now, or 0 for none. Never reused, so a
    /// cancel that arrives late matches nothing rather than matching the wrong
    /// statement.
    job: u64,
    /// The last statement this slot began. An executor begins its jobs in the
    /// order their ids were minted, so an id at or below this one that is not
    /// running has finished, and one above it has not begun.
    last: u64,
    /// When that statement began; meaningless while job == 0. The probe
    /// thread reads it to tell wedged workers from merely busy ones.
    started: Instant,
    /// A cancel that arrived before its statement started.
    ///
    /// The gap is small but entirely reachable: a request registers its
    /// `queryId` before handing the job to an executor, so a client that sends
    /// a query and immediately presses Stop can have the cancel land while the
    /// executor is still picking the job up. Without this the cancel would
    /// match nothing and the query would run to completion having been
    /// explicitly cancelled — the worst of both answers.
    pending: Option<u64>,
    /// Set by a canceller, read and cleared by the executor when the statement
    /// ends. A flag rather than a match on DuckDB's error text: "Interrupted"
    /// is a message, not an interface, and a client should not learn why its
    /// query stopped from prose that may be reworded upstream.
    cancelled: bool,
    /// When this statement must stop, if anything asked for a limit.
    deadline: Option<Instant>,
    /// While this worker handles an HTTP request with no statement of its
    /// own running, when it counts as wedged: `WEDGED_REQUEST_AGE` after it
    /// took the request, or `WEDGED_STATEMENT_AGE` after it handed the
    /// statement to a session's connection, where it waits as a worker
    /// running one does.
    ///
    /// A worker reading a request body has no job — `job` is still 0 — so to
    /// the probe thread it looked idle while being entirely stuck. That is not
    /// a corner case: every denial of service found against this server has
    /// worked by occupying workers BEFORE the statement starts, and the one
    /// thread whose purpose is staying reachable under saturation sat every
    /// one of them out because it was only ever looking at statements.
    wedged_at: Option<Instant>,
}

/// What a cancel request should do, decided from bookkeeping alone, so the
/// part that is easy to get wrong is tested without an engine.
#[derive(Debug, PartialEq, Eq)]
enum Cancel {
    /// Interrupt the connection now.
    Fire,
    /// The statement has not started; the cancel is held for it.
    Held,
    /// Nothing here to cancel.
    Nothing,
}

impl SlotRun {
    fn idle() -> Self {
        SlotRun { job: 0, last: 0, started: Instant::now(), pending: None, cancelled: false, deadline: None, wedged_at: None }
    }

    /// Claim this slot for `job`. Returns true when the statement was already
    /// cancelled before it began, in which case it must not run at all.
    fn begin(&mut self, job: u64, deadline: Option<Instant>) -> bool {
        self.job = job;
        self.last = job;
        self.started = Instant::now();
        self.deadline = deadline;
        // Any held cancel is consumed here whether or not it matches: it named
        // a statement that is now either this one or one that will never start,
        // and either way it has had its say.
        self.cancelled = self.pending.take() == Some(job);
        self.cancelled
    }

    fn end(&mut self) -> bool {
        self.job = 0;
        self.deadline = None;
        std::mem::replace(&mut self.cancelled, false)
    }

    fn arm(&mut self, job: Option<u64>) -> Cancel {
        match job {
            // Named a statement this slot has finished: there is nothing to
            // stop, and the client is told so, whatever it is still reading.
            Some(want) if self.job != want && want <= self.last => Cancel::Nothing,
            // Named one it has not begun: held for it, so it never runs. One
            // that never reaches this slot is discarded by the next `begin`.
            Some(want) if self.job != want => {
                self.pending = Some(want);
                Cancel::Held
            }
            _ if self.job == 0 => Cancel::Nothing,
            _ => {
                self.cancelled = true;
                Cancel::Fire
            }
        }
    }

    fn expired(&self, now: Instant) -> bool {
        self.job != 0 && self.deadline.is_some_and(|d| now >= d)
    }
}

impl SlotState {
    fn new(interrupt: Arc<InterruptHandle>) -> Arc<Self> {
        Arc::new(SlotState { interrupt, run: Mutex::new(SlotRun::idle()) })
    }

    fn begin(&self, job: u64, deadline: Option<Instant>) -> bool {
        self.run.lock().unwrap().begin(job, deadline)
    }

    /// Retire the statement and report whether it was cancelled. Takes the same
    /// lock a canceller holds across its interrupt, which is what closes the
    /// window described above: once this returns, no interrupt aimed at this
    /// job can still be in flight.
    fn end(&self) -> bool {
        self.run.lock().unwrap().end()
    }

    /// Whether a canceller has reached the statement now running.
    fn cancelled(&self) -> bool {
        self.run.lock().unwrap().cancelled
    }

    /// Interrupt the running statement if it is still `job` — or whatever is
    /// running, when `job` is None. Returns whether it stopped, or will stop,
    /// a statement.
    fn cancel(&self, job: Option<u64>) -> bool {
        let mut run = self.run.lock().unwrap();
        match run.arm(job) {
            Cancel::Nothing => false,
            Cancel::Held => true,
            Cancel::Fire => {
                // Under the lock, deliberately. `interrupt()` takes its own
                // mutex and sets a flag through the C API; it cannot re-enter
                // harbor, so there is no lock-order hazard, and firing it
                // outside the lock would reopen the race this whole design
                // exists to close.
                //
                // `duckdb_interrupt` is resolved from the host's
                // function-pointer table and asserts if the host did not
                // provide it. Every DuckDB harbor can load into has, but a
                // panic here would poison this mutex and take cancellation out
                // for the whole process, so it is caught: a harbor that cannot
                // cancel is much better than one that dies trying.
                let fired =
                    std::panic::catch_unwind(AssertUnwindSafe(|| self.interrupt.interrupt()));
                if fired.is_err() {
                    eprintln!(
                        "harbor: this DuckDB does not provide duckdb_interrupt; cannot cancel"
                    );
                    run.cancelled = false;
                    return false;
                }
                true
            }
        }
    }

    /// The running job's id if it has outlived its deadline, else None. Read
    /// and returned together so the caller can cancel exactly that job.
    fn expired_job(&self, now: Instant) -> Option<u64> {
        let run = self.run.lock().unwrap();
        run.expired(now).then_some(run.job)
    }

    /// The job running right now (0 = idle). A snapshot for reapers that must
    /// name their target instead of firing at "whatever is running".
    fn current_job(&self) -> u64 {
        self.run.lock().unwrap().job
    }
}

/// Every executor's slot, worker and lease alike, for the life of the server.
static SLOTS: Mutex<Vec<Arc<SlotState>>> = Mutex::new(Vec::new());

/// Where a cancel lands: the slot the statement occupies, and the job id of
/// this particular run on it. The id matters as much as the slot —
/// cancelling by slot alone races a statement that finished in the meantime
/// and takes down whatever was issued next. (`Cancellable`, below, is the
/// registration guard that puts one of these in the map and takes it out
/// again; this is the value it stores.)
type CancelTarget = (Arc<SlotState>, u64);

/// Statements a client asked to be able to cancel, by the id it chose.
static QUERIES: Mutex<Option<HashMap<String, CancelTarget>>> = Mutex::new(None);

/// Process-unique, monotonic, never reused. Zero means "nothing running", so
/// ids start at one.
static NEXT_JOB: AtomicU64 = AtomicU64::new(1);

fn next_job_id() -> u64 {
    NEXT_JOB.fetch_add(1, Ordering::SeqCst)
}

/// The longest a statement may run, when nothing asks for something shorter.
///
/// Unset by default, and that is a decision rather than an omission: harbor
/// streams 300,000-row results and is used for analytical queries that take
/// minutes on purpose, so a default deadline would break correct programs to
/// protect against incorrect ones. `HARBOR_STATEMENT_TIMEOUT_MS` turns it on
/// for a deployment; `timeoutMs` on the request turns it on for one statement,
/// which is what a console with a Stop button actually wants.
fn configured_statement_timeout() -> Option<Duration> {
    // Read once: the env cannot legitimately change after start, and this is
    // on the per-request path.
    static CONFIGURED: OnceLock<Option<Duration>> = OnceLock::new();
    *CONFIGURED.get_or_init(|| {
        match std::env::var("HARBOR_STATEMENT_TIMEOUT_MS").ok()?.trim().parse::<u64>() {
            Ok(0) | Err(_) => None,
            Ok(ms) => Some(Duration::from_millis(ms)),
        }
    })
}

/// A `queryId` registered for the length of one statement. Dropping it
/// deregisters, on every path out — including the early returns — so a cancel
/// can never reach a statement that has already finished, and the map cannot
/// grow without bound.
struct Cancellable {
    id: String,
}

impl Cancellable {
    fn register(id: &str, slot: &Arc<SlotState>, job: u64) -> Result<Self, Refusal> {
        let mut guard = QUERIES.lock().unwrap();
        let Some(queries) = guard.as_mut() else {
            return Err(Refusal::not_serving());
        };
        if queries.contains_key(id) {
            // Refuse rather than overwrite. Two live statements under one name
            // means a cancel is a coin flip, and silently replacing the first
            // would make the first uncancellable for as long as it runs.
            return Err(Refusal {
                status: 409,
                code: code::QUERY_ID_IN_USE,
                message: format!("queryId {id:?} is already running a statement. Choose another."),
            });
        }
        queries.insert(id.to_string(), (Arc::clone(slot), job));
        Ok(Self { id: id.to_string() })
    }
}

impl Drop for Cancellable {
    fn drop(&mut self) {
        let mut guard = QUERIES.lock().unwrap();
        if let Some(queries) = guard.as_mut() {
            queries.remove(&self.id);
        }
    }
}

/// Cancel by the id the client chose. The slot and job are looked up together,
/// so a `queryId` reused after its statement finished cancels nothing.
fn cancel_query(id: &str) -> bool {
    let target = {
        let guard = QUERIES.lock().unwrap();
        guard.as_ref().and_then(|q| q.get(id).map(|(s, j)| (Arc::clone(s), *j)))
    };
    match target {
        Some((slot, job)) => slot.cancel(Some(job)),
        None => false,
    }
}

/// Stop any statement that has outlived its deadline. Runs on the reaper's
/// tick, so the granularity of a timeout is the reap interval — which is the
/// right trade for a limit measured in seconds and enforced by a thread that
/// would otherwise be asleep.
fn cancel_expired() {
    let slots: Vec<Arc<SlotState>> = SLOTS.lock().unwrap().clone();
    let now = Instant::now();
    for slot in slots {
        // By id, never "whatever is running": between noticing the expiry and
        // firing, the expired statement can finish and a fresh one begin, and
        // a cancel(None) would kill that innocent — the exact race the job-id
        // machinery exists to close (see SlotRun). A stale id finds nothing.
        if let Some(job) = slot.expired_job(now) {
            slot.cancel(Some(job));
        }
    }
}

// ---------------------------------------------------------------------------
// Leases
//
// A transaction lives on a connection, and HTTP requests do not. A lease is
// the thing that bridges them: a connection pinned to one client until it
// commits, rolls back, or stops answering. This is PgBouncer's transaction
// pooling and ActiveRecord's connection checkout, with an HTTP request where
// they have a socket and a thread.
//
// Three properties make it safe rather than merely possible:
//
//   1. Leases draw from their own connections, never the workers'. A pool that
//      serves both runs out of workers the moment enough clients hold
//      transactions open, and answers nothing at all — which is a deadlock,
//      not a slowdown.
//   2. Every lease has a deadline. HTTP has no reliable close signal, so a
//      client that vanishes mid-transaction is indistinguishable from one that
//      is thinking, and a timer is the only way the connection ever comes
//      back. It is not hygiene: an open write transaction makes CHECKPOINT
//      *fail*, so one abandoned lease would break the shutdown that folds the
//      WAL.
//   3. Connections are conserved. Every lease connection is in `free`, inside
//      a live lease, or counted in `inflight` while it is being handed between
//      the two — so `free + live + inflight == total` holds at every instant.
//      A connection pool has one catastrophic bug, which is a connection that
//      goes out and never comes back, and this is the invariant that makes it
//      impossible to introduce quietly. `/sessions` reports it.
// ---------------------------------------------------------------------------

/// A lease connection, identified by the executor it talks to. `slot` is
/// stable for the life of the server and appears in `/sessions`, so a
/// connection can be followed across the leases that borrow it.
struct LeaseConn {
    slot: usize,
    jobs: mpsc::SyncSender<Job>,
    /// This connection's interrupt, so a session can be cancelled by whoever
    /// holds it — the client releasing it, or the reaper taking it back.
    state: Arc<SlotState>,
}

struct Lease {
    conn: LeaseConn,
    opened: Instant,
    last: Instant,
    lifetime: LeaseLifetime,
    statements: u64,
    /// Whether the last transaction-control statement opened one. This is the
    /// field an operator actually wants: a lease sitting idle is a curiosity,
    /// a lease sitting idle inside a write transaction is why the checkpoint
    /// is failing.
    in_transaction: bool,
    /// A statement is running right now. Two requests naming one lease would
    /// otherwise interleave inside a single transaction, which no client could
    /// reason about; the second is refused.
    busy: bool,
    /// Someone asked to release this lease while it was busy. The statement
    /// owns the connection until it returns, so the release cannot happen
    /// there and then; the reaper finishes it on the next tick. Without this a
    /// client that wanted to stop a long statement had no way to say so — the
    /// DELETE simply reported false and the lease ran on.
    doomed: bool,
}

/// Interactive leases have an absolute deadline, and an idle clock that a
/// statement or a renewal resets. Backups instead prove client liveness with
/// renewals that move the deadline; SQL activity alone never does.
struct LeaseLifetime {
    deadline: Instant,
    renewal_ttl: Option<Duration>,
}

impl LeaseLifetime {
    fn new(now: Instant, ttl: Duration, backup: bool) -> Self {
        Self { deadline: now + ttl, renewal_ttl: backup.then_some(ttl) }
    }

    fn expired(&self, now: Instant, last: Instant, busy: bool, idle_ttl: Duration) -> bool {
        now >= self.deadline
            || (self.renewal_ttl.is_none() && !busy && now.duration_since(last) >= idle_ttl)
    }

    /// Renew a lease that has not expired: a backup's deadline, or an
    /// interactive lease's idle clock, whose deadline stays where it was.
    fn renew(&mut self, now: Instant, last: &mut Instant, busy: bool, idle_ttl: Duration) -> bool {
        if self.expired(now, *last, busy, idle_ttl) {
            return false;
        }
        match self.renewal_ttl {
            Some(ttl) => self.deadline = now + ttl,
            None => *last = now,
        }
        true
    }
}

struct Leases {
    free: Vec<LeaseConn>,
    live: HashMap<String, Lease>,
    /// Connections between `free` and `live` — released but not yet rolled
    /// back. Counted so the conservation invariant holds during the handoff
    /// rather than only at rest.
    inflight: usize,
    total: usize,
    idle_ttl: Duration,
    max_ttl: Duration,
}

impl Leases {
    fn accounted(&self) -> usize {
        self.free.len() + self.live.len() + self.inflight
    }
}

static LEASES: Mutex<Option<Leases>> = Mutex::new(None);

/// Ordinary leases expire on statement inactivity or a fixed ceiling. Backup
/// leases instead expire when the client stops renewing, even during SQL.
const LEASE_IDLE_TTL: Duration = Duration::from_secs(30);
const LEASE_MAX_TTL: Duration = Duration::from_secs(300);
const BACKUP_RENEWAL_TTL: Duration = Duration::from_secs(60);
const REAP_INTERVAL: Duration = Duration::from_millis(500);

/// 18 bytes of CSPRNG, hex. Sessions are not a privilege boundary here —
/// every door is machine-local and admits every caller who can reach it —
/// so this is about never colliding and never reusing, not about resisting
/// an attacker who can already dial the server.
fn new_lease_id() -> String {
    let mut bytes = [0u8; 18];
    // Best-effort never happens in practice; the id only has to not collide.
    let _ = getrandom::getrandom(&mut bytes);
    let mut out = String::with_capacity(36);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Replace the engine connection before returning a lease to the free list.
/// This rolls back open work and discards all connection-local state.
///
/// Outside the registry lock, always: this blocks on the executor, and holding
/// the lock across it would serialise every other lease behind whatever this
/// connection is doing.
fn quiesce(conn: &LeaseConn) {
    let (mut job, ready, _) = Job::new(String::new(), Vec::new(), Shape::Ndjson, None);
    job.reset = true;
    if conn.jobs.send(job).is_err() {
        return;
    }
    // Wait for it: a connection is not free until it is clean. Not forever,
    // since the reaper and the shutdown wait here too: a statement that does
    // not unwind in 5 s is left to finish, and the reset queued behind it
    // runs before anything the connection's next holder sends.
    let _ = ready.recv_timeout(Duration::from_secs(5));
}

/// Open a lease, or say why not. `Err` carries the status and body to send.
fn lease_open(requested_ttl: Option<Duration>, backup: bool) -> Result<(String, Duration, Duration), Refusal> {
    let mut guard = LEASES.lock().unwrap();
    let Some(leases) = guard.as_mut() else {
        return Err(Refusal::not_serving());
    };
    if leases.total == 0 {
        return Err(Refusal {
            status: 503,
            code: code::NO_LEASE_CONNECTIONS,
            message: "this harbor has no connections left over for transactions: every one is a \
                      worker. Raise HARBOR_POOL_SIZE above the worker count, or lower workers."
                .to_string(),
        });
    }
    let max_ttl = if backup { BACKUP_RENEWAL_TTL } else { leases.max_ttl };
    let ttl = requested_ttl.unwrap_or(max_ttl).min(max_ttl);
    let idle_ttl = if backup { Duration::ZERO } else { leases.idle_ttl };
    let Some(conn) = leases.free.pop() else {
        return Err(Refusal {
            status: 503,
            code: code::NO_LEASE_AVAILABLE,
            message: format!(
                "all {} transaction connections are in use. Retry, or raise HARBOR_POOL_SIZE.",
                leases.total
            ),
        });
    };
    let now = Instant::now();
    let lifetime = LeaseLifetime::new(now, ttl, backup);
    let id = new_lease_id();
    leases.live.insert(
        id.clone(),
        Lease {
            conn,
            opened: now,
            last: now,
            lifetime,
            statements: 0,
            in_transaction: false,
            busy: false,
            doomed: false,
        },
    );
    Ok((id, ttl, idle_ttl))
}

/// Renewals use only the registry lock, so the probe lane can serve them
/// while SQL workers are occupied, and a renewal runs no statement: it
/// counts in nothing and needs no claim, so it never meets the client's next
/// statement. Released or expired leases stay dead: one that has idled out
/// is gone even before the reaper takes it.
fn lease_renew(id: &str) -> Result<(), Refusal> {
    let mut guard = LEASES.lock().unwrap();
    let leases = guard.as_mut().ok_or_else(Refusal::no_session)?;
    let idle_ttl = leases.idle_ttl;
    let lease = leases.live.get_mut(id).ok_or_else(Refusal::no_session)?;
    if lease.doomed || !lease.lifetime.renew(Instant::now(), &mut lease.last, lease.busy, idle_ttl) {
        lease.doomed = true;
        return Err(Refusal::no_session());
    }
    Ok(())
}

/// A path segment as the client meant it: a `queryId` is chosen freely and
/// arrives in the body as itself, but in a path as its percent-encoding
/// (`encodeURIComponent`). A `%` that starts no escape is taken as itself.
fn percent_decoded(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .filter(|h| h.iter().all(u8::is_ascii_hexdigit))
            .and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn renewal_session_id(path: &str) -> Option<&str> {
    path.strip_prefix("/sql/sessions/")?.strip_suffix("/renew")
        .filter(|id| !id.is_empty() && !id.contains('/'))
}

/// How long a statement will wait for a lease that is busy before refusing.
///
/// Not for concurrency — a transaction is a sequence and two statements at
/// once is a client bug. It is for the seam at the end of the previous
/// request: the claim is released after the response is written, so a client
/// that sends its next statement the instant it reads the last byte can arrive
/// while the server is still a few instructions from letting go. That window
/// is microseconds and entirely ours, so waiting it out is right where
/// refusing would be a lie. A genuinely concurrent second statement still gets
/// its 409, just a quarter-second later.
const CLAIM_WAIT: Duration = Duration::from_millis(250);

/// Claim a lease for one statement, waiting out the handoff window above.
///
/// Hands back the slot as well as the channel: a statement on a lease is
/// cancellable by `queryId` exactly like one on a worker, and the registry
/// needs to know which connection to interrupt.
fn lease_claim(id: &str) -> Result<(mpsc::SyncSender<Job>, Arc<SlotState>), Refusal> {
    let deadline = Instant::now() + CLAIM_WAIT;
    loop {
        match try_lease_claim(id) {
            Err(refusal) if refusal.code == code::SESSION_BUSY && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(1));
            }
            other => return other,
        }
    }
}

fn try_lease_claim(id: &str) -> Result<(mpsc::SyncSender<Job>, Arc<SlotState>), Refusal> {
    let mut guard = LEASES.lock().unwrap();
    let Some(leases) = guard.as_mut() else {
        return Err(Refusal::not_serving());
    };
    let Some(lease) = leases.live.get_mut(id) else {
        return Err(Refusal::no_session());
    };
    if lease.doomed || lease.lifetime.expired(Instant::now(), lease.last, lease.busy, leases.idle_ttl) {
        lease.doomed = true;
        // The client asked for this lease's release; honoring new claims while
        // the cancel unwinds would let a "released" session that keeps sending
        // short statements stay busy at every reaper tick — held, with its
        // open transaction, forever. Same answer as absent: it is gone.
        return Err(Refusal::no_session());
    }
    if lease.busy {
        return Err(Refusal {
            status: 409,
            code: code::SESSION_BUSY,
            message: "this session is already running a statement. A transaction is a sequence, \
                      not a pool; send its statements one after another."
                .to_string(),
        });
    }
    lease.busy = true;
    lease.last = Instant::now();
    Ok((lease.conn.jobs.clone(), Arc::clone(&lease.conn.state)))
}

/// Give a claim back, recording what the statement did to the transaction
/// when it ran.
fn lease_settle(id: &str, sql: &str, ran: bool) {
    let mut guard = LEASES.lock().unwrap();
    let Some(leases) = guard.as_mut() else { return };
    let Some(lease) = leases.live.get_mut(id) else { return };
    lease.busy = false;
    lease.last = Instant::now();
    lease.statements += 1;
    if ran && let Some(open) = transaction_effect(sql) {
        lease.in_transaction = open;
    }
}

/// A lease claimed for the length of one request. Dropping it hands the lease
/// back and records what the statement did to the transaction — on every path
/// out of `run_sql`, including the ones that return early.
struct Claim {
    id: String,
    sql: String,
    target: mpsc::SyncSender<Job>,
    state: Arc<SlotState>,
    /// The engine ran the statement: it answered, or refused it as SQL once
    /// past the parser. A statement that never ran (refused before the
    /// engine, cancelled, unparsable) did nothing to the transaction, and a
    /// `BEGIN` among them opened none.
    ran: std::cell::Cell<bool>,
}

impl Drop for Claim {
    fn drop(&mut self) {
        lease_settle(&self.id, &self.sql, self.ran.get());
    }
}

/// Release a lease and return its connection. Idempotent: releasing an already
/// released lease is a no-op that reports false, so a client retrying a DELETE
/// can never free a connection twice or free one that has been reissued.
///
/// A busy lease is not released here — the statement in flight owns the
/// connection until it finishes, and yanking it would hand the same connection
/// to two callers at once. It is cancelled and marked instead, and the reaper
/// releases it as soon as the statement lets go.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Released {
    /// The connection is back in the free list.
    Yes,
    /// No such lease, or it was already gone. Idempotent by design.
    No,
    /// It was running a statement; that statement has been interrupted and the
    /// lease will be released once it unwinds.
    Cancelling,
}

fn lease_release(id: &str) -> Released {
    let lease = {
        let mut guard = LEASES.lock().unwrap();
        let Some(leases) = guard.as_mut() else { return Released::No };
        match leases.live.get_mut(id) {
            Some(l) if l.busy => {
                l.doomed = true;
                // By id, as every cancel is (an idle slot's 0 names nothing).
                // One claimed but not yet begun is the reaper's on its next
                // tick. `cancel` takes only the slot's own lock, so the
                // nesting cannot cycle back here.
                l.conn.state.cancel(Some(l.conn.state.current_job()));
                return Released::Cancelling;
            }
            Some(_) => {}
            None => return Released::No,
        }
        let lease = leases.live.remove(id).expect("checked above");
        leases.inflight += 1;
        lease
    };
    quiesce(&lease.conn);
    let mut guard = LEASES.lock().unwrap();
    if let Some(leases) = guard.as_mut() {
        leases.free.push(lease.conn);
        leases.inflight -= 1;
    }
    Released::Yes
}

/// Reclaim leases that have stopped answering. Two clocks: a lease that has
/// been idle past its idle timeout, and one that has outlived its deadline
/// whatever it has been doing.
///
/// A busy lease cannot be released out from under its statement, so an expired
/// one is cancelled here and released on a later tick, once the statement it
/// was running has come back: a lease wedged inside a runaway statement is the
/// one case where reclaiming actually matters. The idle clock deliberately
/// does not apply to a busy lease:
/// a statement that has been running for a minute is working, not idle.
fn lease_reap() {
    enum Action {
        Release(String),
        Cancel(Arc<SlotState>, u64),
    }
    let actions: Vec<Action> = {
        let mut guard = LEASES.lock().unwrap();
        let Some(leases) = guard.as_mut() else { return };
        let now = Instant::now();
        leases
            .live
            .iter_mut()
            .filter_map(|(id, l)| {
                l.doomed |= l.lifetime.expired(now, l.last, l.busy, leases.idle_ttl);
                match l.busy {
                    // Cancel once per tick while it is over its deadline. Repeating
                    // is deliberate: DuckDB checks the interrupt flag between
                    // pipeline steps, and a statement that swallowed the first one
                    // gets asked again rather than being left to run forever.
                    //
                    // The job id is captured with the decision. Between this
                    // snapshot and the fire below sit other actions, each a
                    // blocking quiesce — plenty of time for the doomed statement
                    // to finish and the connection to be reissued to an innocent.
                    // cancel(Some(job)) makes the late fire a no-op instead of a
                    // random casualty. A statement that has not begun yet (job 0)
                    // waits for the next tick.
                    true if l.doomed => {
                        let job = l.conn.state.current_job();
                        (job != 0).then(|| Action::Cancel(Arc::clone(&l.conn.state), job))
                    }
                    true => None,
                    false if l.doomed => Some(Action::Release(id.clone())),
                    false => None,
                }
            })
            .collect()
    };
    for action in actions {
        match action {
            // Through the same release path as everything else, so a reaped
            // lease and a released one cannot diverge.
            Action::Release(id) => {
                lease_release(&id);
            }
            Action::Cancel(state, job) => {
                state.cancel(Some(job));
            }
        }
    }
}

/// Roll back and return every lease connection. Called during shutdown, before
/// the CHECKPOINT, because an open write transaction makes that checkpoint
/// fail — which would turn a clean stop into a WAL replay on next open.
fn lease_drain() {
    let ids: Vec<String> = {
        let guard = LEASES.lock().unwrap();
        match guard.as_ref() {
            Some(leases) => leases.live.keys().cloned().collect(),
            None => return,
        }
    };
    let mut cancelling = false;
    for id in &ids {
        // A lease busy with a statement is interrupted by this call rather than
        // waited on: the executor's own shutdown unwinds only once the
        // statement finishes, so a single long query would hold the whole
        // shutdown, and with it the CHECKPOINT that folds the WAL.
        cancelling |= lease_release(id) == Released::Cancelling;
    }

    // Bounded patience, the same bargain the worker join makes below. A
    // cancelled statement unwinds on its own thread, and until it does its
    // lease still holds an open transaction — which is exactly what makes the
    // CHECKPOINT fail, so charging straight at it wins nothing. Waiting
    // forever is worse: this runs on the signal thread, so a statement that
    // never unwinds makes the whole process deaf to SIGTERM, and the only way
    // out is the SIGKILL that forfeits the checkpoint this drain exists to
    // reach. So: give it a moment, then go on regardless.
    if cancelling {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            lease_reap();
            let clear = match LEASES.lock().unwrap().as_ref() {
                Some(l) => l.live.is_empty(),
                None => true,
            };
            if clear {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        eprintln!(
            "harbor: a transaction did not unwind in 5s; checkpointing without it \
             (the WAL is intact and replays on next open)"
        );
    }
}

struct Running {
    server: Arc<Server>,
    stop: Arc<AtomicBool>,
    workers: Vec<JoinHandle<Option<Connection>>>,
    /// Lease executors. They have no accept loop — they exist only to run
    /// statements on their own pinned connection — so they end when their
    /// channel is dropped, which is what `stop` does after draining.
    leases: Vec<JoinHandle<Option<Connection>>>,
    reaper: Option<JoinHandle<()>>,
    /// The saturation-proof lane: answers /ready and the control plane when
    /// every worker is busy, and relays sessions' statements. See probe_worker.
    probe: Option<JoinHandle<()>>,
    addr: String,
}

/// Allocate the fixed pool and lock operator settings after initialization.
pub fn open_pool(mut con: Connection) -> Result<(), String> {
    let mut pool = POOL.lock().unwrap();

    // Once per process: POOL and CONTROL are process-wide, and a second
    // database's pool beside the first would leave the shutdown CHECKPOINT on
    // whichever opened last.
    if !pool.is_empty() {
        return Err("harbor's pool is already open in this process, which serves one database".to_string());
    }

    lock_operator_settings(&mut con)?;

    for _ in 0..configured_pool_size() {
        pool.push(con.try_clone().map_err(|e| format!("harbor: {e}"))?);
    }
    // The handle has to be taken while the connection is still here, exactly
    // as `start()` does for the workers.
    *CONTROL_SLOT.lock().unwrap() = Some(SlotState::new(con.interrupt_handle()));
    *CONTROL.lock().unwrap() = Some(con);
    Ok(())
}

// ---------------------------------------------------------------------------
// start / stop / wait
// ---------------------------------------------------------------------------

/// Where the server listens. Unix sockets are the fleet's default face; a
/// port is an additional door, not a different server — Dual keeps the
/// socket (the fleet's registration and the fastest local path) while
/// also answering TCP. Plain Tcp exists for Windows, which has no unix
/// sockets.
///
/// TCP is IPv4 loopback only, always: this process trusts its own machine and
/// nothing else, and anything wider — remote reach, access policy — belongs to an
/// edge proxy in front of it.
pub enum Listen {
    Tcp { port: u16 },
    #[cfg(unix)]
    Unix(std::path::PathBuf),
    #[cfg(unix)]
    Dual { port: u16, sock: std::path::PathBuf },
}

/// The TCP door is deliberately one IPv4 loopback listener. Callers use the
/// literal `127.0.0.1`, so name resolution never changes which address Harbor
/// serves.
fn ipv4_loopback(port: u16) -> Result<Vec<justhttp::Listener>, String> {
    let v4 = std::net::TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| format!("harbor: cannot bind 127.0.0.1:{port}: {e}"))?;
    Ok(vec![justhttp::Listener::from(v4)])
}

pub fn start(listen: Listen, workers: usize, log: bool) -> Result<String, String> {
    let mut running = RUNNING.lock().unwrap();
    if let Some(r) = running.as_ref() {
        return Err(format!("harbor is already serving on {}", r.addr));
    }

    // Take the connections first: binding a socket we then cannot serve on
    // is a worse failure than not binding at all.
    let mut pool = POOL.lock().unwrap();
    if pool.is_empty() {
        return Err("harbor: no database connections (the pool was never opened)".to_string());
    }
    // Say so rather than quietly serving with fewer. workers is capped by the
    // connection pool, which is fixed at load, so `workers := 32` gets what the
    // pool has —
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
    // Everything the workers did not take becomes lease capacity. Nothing is
    // held back for later: a connection that is neither serving requests nor
    // available for a transaction is doing nothing at all, and it cannot be
    // created on demand, so there is no reason to keep one in reserve.
    let mut lease_conns: Vec<Connection> = pool.drain(..).collect();
    drop(pool);

    let bound = match &listen {
        Listen::Tcp { port } => ipv4_loopback(*port).and_then(|doors| {
            Server::serve(doors).map_err(|e| format!("harbor: cannot serve: {e}"))
        }),
        #[cfg(unix)]
        Listen::Unix(path) => Server::http_unix(path.as_path())
            .map_err(|e| format!("harbor: cannot bind {}: {e}", path.display())),
        #[cfg(unix)]
        Listen::Dual { port, sock } => ipv4_loopback(*port).and_then(|mut doors| {
            let unix = std::os::unix::net::UnixListener::bind(sock)
                .map_err(|e| format!("harbor: cannot bind {}: {e}", sock.display()))?;
            doors.push(justhttp::Listener::from(unix));
            Server::serve(doors).map_err(|e| format!("harbor: cannot serve: {e}"))
        }),
    };
    let server = match bound {
        Ok(s) => s,
        Err(msg) => {
            let mut pool = POOL.lock().unwrap();
            pool.append(&mut conns);
            pool.append(&mut lease_conns);
            return Err(msg);
        }
    };
    let addr = match &listen {
        Listen::Tcp { .. } => server.server_addr().to_string(),
        #[cfg(unix)]
        Listen::Unix(path) => path.display().to_string(),
        #[cfg(unix)]
        Listen::Dual { sock, .. } => {
            format!("{} + {}", server.server_addr(), sock.display())
        }
    };
    *STARTED_AT.lock().unwrap() = Some(Instant::now());
    // Reset the process-global readiness verdict for this instance. One
    // process may start, stop, and start again (tests do exactly this), and
    // a fresh instance must not inherit a stale verdict from its predecessor.
    *LAST_READY.lock().unwrap() = None;
    let server = Arc::new(server);
    *STOPPED.0.lock().unwrap() = false;
    let mut r = Running {
        server: Arc::clone(&server),
        stop: Arc::new(AtomicBool::new(false)),
        workers: Vec::new(),
        leases: Vec::new(),
        reaper: None,
        probe: None,
        addr: addr.clone(),
    };
    let spawned = spawn_threads(&mut r, conns, lease_conns, log);
    *running = Some(r);
    *SERVER.lock().unwrap() = Some(server);
    if let Err(e) = spawned {
        // What did start stops the ordinary way, so nothing is left serving
        // that nothing can stop, and the connections go back to the pool.
        drop(running);
        let _ = stop();
        return Err(format!("harbor: cannot start a thread: {e}"));
    }
    Ok(addr)
}

/// Every thread of a server, into `r` as each starts: the workers, an
/// executor per lease connection, the reaper and the probe lane.
fn spawn_threads(
    r: &mut Running,
    conns: Vec<Connection>,
    lease_conns: Vec<Connection>,
    log: bool,
) -> std::io::Result<()> {
    // Every executor gets a slot before it gets a thread. The interrupt handle
    // has to be taken from the connection while it is still here — an executor
    // owns its connection for the life of the server and nothing else can
    // reach it afterwards.
    let workers = conns.len();
    let mut slots: Vec<Arc<SlotState>> = Vec::with_capacity(workers + lease_conns.len());
    for (i, conn) in conns.into_iter().enumerate() {
        let server = Arc::clone(&r.server);
        let stop = Arc::clone(&r.stop);
        let state = SlotState::new(conn.interrupt_handle());
        slots.push(Arc::clone(&state));
        r.workers.push(
            thread::Builder::new()
                .name(format!("harbor-{i}"))
                .spawn(move || worker(server, stop, conn, state, log))?,
        );
    }

    // One executor per lease connection, each owning it for the life of the
    // server. A lease borrows the executor, not the thread: statements arrive
    // from whichever worker accepted the request and are answered here, which
    // is what keeps a transaction on one connection without taking a worker
    // out of the accept loop to babysit it.
    let mut free = Vec::with_capacity(lease_conns.len());
    for (slot, conn) in lease_conns.into_iter().enumerate() {
        // Capacity 1 for the same reason as the worker executors: one job
        // outstanding by construction, so the slot only skips a double park.
        let (tx, rx) = mpsc::sync_channel::<Job>(1);
        let state = SlotState::new(conn.interrupt_handle());
        slots.push(Arc::clone(&state));
        let exec_state = Arc::clone(&state);
        r.leases.push(
            thread::Builder::new()
                .name(format!("harbor-lease-{slot}"))
                .stack_size(EXEC_STACK)
                .spawn(move || Some(execute_jobs(conn, rx, true, exec_state)))?,
        );
        free.push(LeaseConn { slot, jobs: tx, state });
    }
    *WORKER_SLOTS.lock().unwrap() = slots[..workers].to_vec();
    // Appended last, after the workers and the leases, so the worker window
    // above is untouched. CONTROL is not a worker and must never make the
    // probe thread think one is wedged.
    if let Some(control) = CONTROL_SLOT.lock().unwrap().clone() {
        slots.push(control);
    }
    *SLOTS.lock().unwrap() = slots;
    *QUERIES.lock().unwrap() = Some(HashMap::new());
    let total = free.len();
    *LEASES.lock().unwrap() = Some(Leases {
        free,
        live: HashMap::new(),
        inflight: 0,
        total,
        idle_ttl: LEASE_IDLE_TTL,
        max_ttl: LEASE_MAX_TTL,
    });

    // The reaper is what makes a lease safe to hand out at all: without it an
    // abandoned transaction holds its connection until the process exits, and
    // blocks every checkpoint in between. It also enforces statement
    // deadlines, which apply to the workers as well.
    let stop = Arc::clone(&r.stop);
    r.reaper = Some(thread::Builder::new().name("harbor-reaper".to_string()).spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            thread::sleep(REAP_INTERVAL);
            cancel_expired();
            lease_reap();
        }
    })?);

    // One thread the fleet can always reach. Workers pair 1:1 with
    // connections and stream whole responses, so when every worker is busy an
    // accepted /ready sits in justhttp's queue until one frees — measured at
    // 5 seconds under a saturating analytical load — and a load balancer with
    // an ordinary timeout marks a busy-but-healthy berth dead precisely when
    // killing it hurts most. This thread never runs a statement of its own:
    // /ready is answered from the CONTROL connection, a session's statement
    // is relayed to the session's own connection, and work that needs a
    // worker gets an immediate honest 503 — shedding load instead of
    // queueing it invisibly.
    let (server, stop) = (Arc::clone(&r.server), Arc::clone(&r.stop));
    r.probe = Some(
        thread::Builder::new()
            .name("harbor-probe".to_string())
            .spawn(move || probe_worker(server, stop, log))?,
    );
    Ok(())
}

/// The running server, readable apart from RUNNING, which a stop holds for
/// as long as it drains.
static SERVER: Mutex<Option<Arc<Server>>> = Mutex::new(None);

/// Client connections on the running server: (live now, accepted since it
/// started), or None when nothing is serving. This is the whole lifetime
/// signal for a refcounted (spawned-on-use) server: the host leaves once none
/// has been live past the grace windows — the startup one until a first
/// client has come, however briefly. Idle keep-alive connections count — an
/// attached client, even a quiet one, is a claim.
pub fn connections() -> Option<(usize, usize)> {
    SERVER.lock().unwrap().as_ref().map(|s| (s.connection_count(), s.accepted_count()))
}

/// The TCP door's port, when the server has one: the port asked for, or the
/// one the system chose for port 0.
pub fn tcp_port() -> Option<u16> {
    match SERVER.lock().unwrap().as_ref()?.server_addr() {
        justhttp::ListenAddr::Ip(addr) => Some(addr.port()),
        #[cfg(unix)]
        justhttp::ListenAddr::Unix(_) => None,
    }
}

pub fn stop() -> Result<String, String> {
    stop_with(RUNNING.lock().unwrap())
}

/// Stop the server only if no client has connected since `accepted` had been
/// accepted and none is connected now: a refcounted server's departure. The
/// last look and the start of the stop are one step under the server's lock,
/// so a client that arrived since the host last looked keeps the server.
pub fn stop_if_idle(accepted: usize) -> bool {
    let running = RUNNING.lock().unwrap();
    if connections() != Some((0, accepted)) {
        return false;
    }
    let _ = stop_with(running);
    true
}

fn stop_with(mut running: std::sync::MutexGuard<'_, Option<Running>>) -> Result<String, String> {
    // Held for the whole of the shutdown, not just the take(). Releasing it
    // here — which `RUNNING.lock().unwrap().take()` as a statement does, since
    // the guard is a temporary — leaves a window in which RUNNING is None while
    // the listener is still bound and the workers are still draining. A
    // start() arriving in that window sees no server, takes whichever
    // connections happen to be back in the pool, and then fails to bind a port
    // the old listener has not released yet.
    let Some(r) = running.take() else {
        return Err("harbor is not serving".to_string());
    };
    // On a server only its unix socket reaches, a client that arrives from
    // here on is refused at connect, so it knows its request was never sent;
    // one accepted and then left unanswered could not tell whether its
    // statement ran. The socket file stays until the server is gone, which
    // is what a stop over it waits for. A TCP door stays open through the
    // drain: a stop by URL reads a refused port as a server that is gone.
    #[cfg(unix)]
    if matches!(r.server.server_addr(), justhttp::ListenAddr::Unix(_)) {
        r.server.close_doors();
    }
    r.stop.store(true, Ordering::SeqCst);
    r.server.unblock();
    // What still reaches the queue, through the TCP door or on a connection
    // already open, is answered rather than left to the process's exit, and
    // the answer says that nothing of it ran. The thread lets go of the
    // server between requests, so it ends when the server does.
    let server = Arc::downgrade(&r.server);
    let _ = thread::Builder::new().name("harbor-stopping".to_string()).spawn(move || {
        while let Some(next) = server.upgrade().and_then(|s| s.recv_timeout(Duration::from_millis(50)).ok()) {
            if let Some(req) = next {
                let _ = req.respond(error_response(503, code::UNAVAILABLE,
                    "harbor is stopping; this statement did not run"));
            }
        }
    });

    // Before anything else: roll back every live transaction. This is not
    // tidiness. An open write transaction makes CHECKPOINT fail outright —
    // "there are other write transactions active" — so a single client that
    // opened a transaction and wandered off would turn the clean stop below
    // into a WAL replay on next open. Draining first is what makes the
    // checkpoint reachable.
    lease_drain();
    // Dropping the registry drops the free list, and with it every sender.
    // The lease executors see their channel close, roll back once more on the
    // way out, and hand their connection back through the join below.
    let leases = LEASES.lock().unwrap().take();
    drop(leases);

    // A statement still running on a worker holds a connection this shutdown
    // is about to wait on, so ask every one of them to stop. Without this a
    // single long query decides how long the stop takes — and the CHECKPOINT
    // that folds the WAL is on the other side of it.
    let slots: Vec<Arc<SlotState>> = std::mem::take(&mut *SLOTS.lock().unwrap());
    for slot in &slots {
        slot.cancel(None);
    }
    drop(slots);
    // Nothing can be cancelled by name once the registry is gone, and an id
    // left behind would outlive the server that could act on it.
    *QUERIES.lock().unwrap() = None;

    // Workers hand their connection back as they exit, so a later
    // start() has a pool to draw from. A panicked worker forfeits its
    // connection rather than taking the shutdown down with it.
    //
    // Bounded patience, not join(): a worker whose client stopped reading is
    // stuck inside a socket write — justhttp caps a stalled write at ~10s,
    // longer than this drain is willing to wait — and a
    // plain join would wait on it forever. That wedged this whole function:
    // the signal thread sat inside stop(), the second SIGTERM queued behind
    // the RUNNING mutex, and the only exit left was SIGKILL — which forfeits
    // the CHECKPOINT this drain exists to reach. After the deadline the
    // straggler is abandoned exactly as a panicked worker would be: its
    // connection is forfeited, and the checkpoint below runs regardless.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut pool = POOL.lock().unwrap();
    let mut pending: Vec<thread::JoinHandle<Option<Connection>>> =
        r.workers.into_iter().chain(r.leases).collect();
    loop {
        let (done, rest): (Vec<_>, Vec<_>) = pending.into_iter().partition(|h| h.is_finished());
        for h in done {
            if let Ok(Some(conn)) = h.join() {
                pool.push(conn);
            }
        }
        pending = rest;
        if pending.is_empty() || Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    if !pending.is_empty() {
        eprintln!(
            "harbor: {} executor(s) still writing to stalled clients; abandoning them to checkpoint",
            pending.len()
        );
    }
    drop(pool);
    if let Some(h) = r.reaper {
        // Prompt: the reaper sleeps in short ticks and checks the stop flag.
        let _ = h.join();
    }
    if let Some(h) = r.probe {
        // Same bounded patience as the workers: joined if it made it out of
        // its recv loop, abandoned if it is wedged writing to a dead client.
        if h.is_finished() {
            let _ = h.join();
        }
    }

    // Fold the WAL back into the database file so the next open needs no
    // replay. By the time we reach here the leases are drained and the workers
    // are joined (above), so no write transaction is open and this should
    // succeed. A failure is therefore a real signal — a full or failing disk,
    // most likely — not a routine outcome to swallow: the database is still
    // safe (the WAL is intact and replays on next open) but the restart is
    // slower and the operator should know why, so surface it instead of
    // reporting a clean "drained and checkpointed" shutdown that did not fully
    // happen.
    if let Some(c) = CONTROL.lock().unwrap().as_mut()
        && let Err(e) = c.execute_batch("CHECKPOINT")
    {
        eprintln!(
            "harbor: shutdown CHECKPOINT failed ({e}); the WAL is intact and \
             will replay on next open (no data lost, slower restart)"
        );
    }

    *SERVER.lock().unwrap() = None;
    let (lock, cv) = &STOPPED;
    *lock.lock().unwrap() = true;
    cv.notify_all();
    drop(running);
    Ok(r.addr)
}

/// Turn SIGTERM, SIGINT and SIGHUP — on Windows, Ctrl-C and Ctrl-Break —
/// into a clean `stop()`.
///
/// Registered from `wait()` and nowhere else. `wait()` is what makes the
/// process a daemon — nothing else is going to happen on the main thread —
/// so that is the one moment where taking over the signals is harbor's call
/// to make. In an ordinary interactive session the CLI keeps its own Ctrl-C,
/// which cancels a query rather than shutting the database down.
///
/// Without this, a `kill`, or the hangup of a closed terminal or a dropped
/// ssh session, runs the default handler: the process dies with the WAL
/// unfolded and the next open has to replay it.
fn install_signal_handler() {
    use signal_hook::consts::{SIGINT, SIGTERM};

    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    #[cfg(unix)]
    let signals = [SIGTERM, SIGINT, signal_hook::consts::SIGHUP];
    #[cfg(windows)]
    let signals = [SIGTERM, SIGINT, signal_hook::consts::SIGBREAK];
    let asked = Arc::new(AtomicBool::new(false));
    for signal in signals {
        let _ = signal_hook::flag::register(signal, Arc::clone(&asked));
    }
    // The handlers stay registered for the life of the process, so a second
    // signal — a supervisor escalating after a timeout, an impatient second
    // Ctrl-C — asks again rather than killing the process with the WAL
    // unfolded while the launcher's CHECKPOINT runs after wait() returns.
    // stop() is idempotent enough to call again: the second call finds
    // RUNNING empty and returns an error nobody reads.
    let _ = thread::Builder::new().name("harbor-signals".to_string()).spawn(move || {
        loop {
            thread::sleep(Duration::from_millis(100));
            if asked.swap(false, Ordering::SeqCst) {
                // stop() drains the workers and checkpoints, then wakes
                // wait(), which lets the main thread exit normally.
                let _ = stop();
            }
        }
    });
}

/// Block until the server stops. Returns the address it was serving on.
pub fn wait() -> Result<String, String> {
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
/// The split is what makes keep-alive possible. justhttp will frame a
/// response of unknown length itself — chunked, connection reusable — but
/// only if it is handed a `Read` to pull from. A query cannot be that `Read`:
/// the rows come from a borrow chain rooted in a `Connection` that is not
/// `Sync`. Putting the connection on its own thread and passing byte chunks
/// through a bounded channel gives justhttp its reader and keeps the query
/// streaming. A connection closed after every response would cost a client
/// an ephemeral port per request, held for the TIME_WAIT interval — about 16k
/// ports over 30s on macOS — and one client at a few thousand requests a
/// second would run out in seconds.
fn worker(
    server: Arc<Server>,
    stop: Arc<AtomicBool>,
    conn: Connection,
    state: Arc<SlotState>,
    log: bool,
) -> Option<Connection> {
    // Capacity 1, not a rendezvous: a worker never has more than one
    // statement outstanding (it waits on `ready` before its next request), so
    // nothing can queue — but the buffer slot lets the sender hand off and
    // proceed straight to that wait instead of parking twice per request.
    let (jobs_tx, jobs_rx) = mpsc::sync_channel::<Job>(1);
    let exec_state = Arc::clone(&state);
    let executor = thread::Builder::new()
        .name("harbor-exec".to_string())
        .stack_size(EXEC_STACK)
        .spawn(move || execute_jobs(conn, jobs_rx, false, exec_state))
        .ok()?;

    while !stop.load(Ordering::SeqCst) {
        // A timeout rather than a blocking recv, so `unblock()` is not the
        // only way out and a worker cannot wedge on shutdown.
        match server.recv_timeout(Duration::from_millis(200)) {
            // A worker whose executor has died must leave the accept loop. All
            // workers pull from one shared queue, so one that answers instantly
            // — which is what a worker with no executor does, 503 by return —
            // wins races against every worker still doing real work, and
            // absorbs a growing share of the traffic. `/ready` reports it — it
            // runs a real query, so a dead executor answers 503 — but
            // reporting it is not enough: the worker still has to leave.
            Ok(Some(req)) => {
                if !handle(req, Some((&jobs_tx, &state)), log) {
                    break;
                }
            }
            Ok(None) => continue,
            // The listener is gone — justhttp only surfaces an accept error
            // once it has decided the socket itself is unusable (transient
            // failures are retried there). This berth will never accept
            // another connection, so it says so: a process alive and holding
            // the database and the flock looks healthy to a supervisor
            // watching the pid while every client sees connection-refused.
            Err(e) => {
                eprintln!(
                    "harbor: the listener has failed ({e}); this berth can no longer \
                     accept connections. Stop it and start a new one."
                );
                break;
            }
        }
    }

    drop(jobs_tx);
    executor.join().ok()
}

/// The saturation-proof lane (see the note in `start`): the control plane
/// that must stay reachable precisely when every worker is busy. /ready so a
/// load balancer never mistakes busy for dead; cancels and releases because
/// they are how a saturated berth gets UN-saturated; /sessions and /info
/// because an operator debugging the saturation needs them. All bounded,
/// in-memory responses — this thread never reads a body, streams or borrows
/// a connection, so a client that stops reading can wedge a worker but not
/// the berth's last open door. A session's statement is relayed to a thread
/// of its own (`relay`), since it runs on the session's connection; other
/// statements and /catalog get a fast honest 503 instead of queueing
/// invisibly behind the analytics.
fn probe_worker(server: Arc<Server>, stop: Arc<AtomicBool>, log: bool) {
    while !stop.load(Ordering::SeqCst) {
        // Only join the accept queue when the workers are WEDGED — every one
        // of them mid-statement for at least 250ms — not merely busy. All
        // recv() callers share one queue, so a probe that listened while
        // workers were healthy would win requests from them and shed load
        // nobody needed shed; and a storm of quick queries keeps all workers
        // "busy" while serving thousands per second, which is queueing
        // working as designed. Six multi-second analytics is the situation
        // this thread exists for, and statement age is what tells the two
        // apart. (Verified against the stress suite: 16 fast clients, zero
        // sheds; 6 slow scans, probe live within a quarter second.)
        if !workers_wedged() {
            thread::sleep(Duration::from_millis(25));
            continue;
        }
        match server.recv_timeout(Duration::from_millis(100)) {
            Ok(Some(req)) => {
                let _ = handle(req, None, log);
            }
            Ok(None) => continue,
            // Same as a worker: the listener is gone. Whichever thread pops
            // the error reports it; the rest simply stop seeing requests.
            Err(e) => {
                eprintln!(
                    "harbor: the listener has failed ({e}); this berth can no longer \
                     accept connections. Stop it and start a new one."
                );
                break;
            }
        }
    }
}

/// `/ready` as the probe thread answers it: the recent cached verdict, else
/// SELECT 1 on the CONTROL connection. Shallower than the workers' full-path
/// probe — but the question under saturation is "is the database alive",
/// not "is a worker free", and a probe that queues behind the workers turns
/// a busy berth into a dead one in the eyes of its load balancer.
fn run_ready_control(req: Request) -> (bool, u16) {
    if let Some((at, ok)) = *LAST_READY.lock().unwrap()
        && at.elapsed() < READY_MAX_AGE
    {
        return (true, respond_ready(req, ok, "not ready"));
    }
    // Registered on CONTROL's slot for the length of the query, so a
    // readiness probe that wedges can be interrupted instead of holding the
    // connection — and the mutex `stop()` wants — indefinitely. No deadline:
    // a probe harbor cancelled itself would report the database unready when
    // the database was fine, which is the same reasoning as `run_ready`.
    let slot = CONTROL_SLOT.lock().unwrap().clone();
    let ok = {
        let mut guard = CONTROL.lock().unwrap();
        let job = next_job_id();
        let _on_slot = slot.as_ref().map(|s| {
            s.begin(job, None);
            OnSlot { slot: s, done: false }
        });
        guard.as_mut().is_some_and(|c| c.execute_batch("SELECT 1").is_ok())
    };
    *LAST_READY.lock().unwrap() = Some((Instant::now(), ok));
    (true, respond_ready(req, ok, "not ready"))
}

/// How long a worker's statement must have run, on its own connection or on
/// a session's, before the worker counts as wedged.
const WEDGED_STATEMENT_AGE: Duration = Duration::from_millis(250);

/// How long a worker must be stuck on a request that has NOT become a
/// statement before it counts as wedged.
///
/// Deliberately far longer than the statement threshold, and the asymmetry is
/// the point. A statement still running after 250ms while every worker is busy
/// is the analytical load this lane was built for. A request body still
/// arriving after five seconds is not load — a real body is one SQL statement
/// and lands in milliseconds — it is a client that has stopped making
/// progress. Keeping the two thresholds apart catches the stuck case without
/// re-tuning the busy case the stress lane pins (16 fast clients, zero sheds).
const WEDGED_REQUEST_AGE: Duration = Duration::from_secs(5);

/// Every worker occupied, and every one of them occupied long enough to mean
/// it. See the probe loop for why age is the discriminator.
///
/// "Occupied" is not "running a statement": a worker held in a request body —
/// draining one nobody read, or waiting on one dribbling in a byte at a time —
/// has no job, and six such workers are a berth that answers nothing. A worker
/// is occupied from the moment it picks up a request; whether that request
/// ever reaches DuckDB is a distinction the load balancer does not care about.
fn workers_wedged() -> bool {
    let slots = WORKER_SLOTS.lock().unwrap();
    !slots.is_empty()
        && slots.iter().all(|s| {
            let run = s.run.lock().unwrap();
            match run.job != 0 {
                true => run.started.elapsed() >= WEDGED_STATEMENT_AGE,
                false => run.wedged_at.is_some_and(|t| Instant::now() >= t),
            }
        })
}

/// The probe thread's answer to work it cannot take: immediate and honest,
/// instead of an invisible seat in the queue behind the analytics.
fn shed(req: Request) -> (bool, u16) {
    let _ = req.respond(error_response(503, code::UNAVAILABLE, "every worker is busy; retry shortly"));
    (true, 503)
}

// ---------------------------------------------------------------------------
// Berth identity (GET /info) and refcounted idle exit (HARBOR_EPHEMERAL)
// ---------------------------------------------------------------------------

/// Identity document the embedding host sets before `start()`; GET /info
/// serves it with `uptimeMs` spliced in. The host owns the static fields
/// (name, database path, pid) because the core cannot know them.
/// Unset, /info answers 404, which a client reads as a server that does not
/// speak it.
static INFO: Mutex<Option<serde_json::Value>> = Mutex::new(None);
static STARTED_AT: Mutex<Option<Instant>> = Mutex::new(None);
/// The workers' slots alone (SLOTS holds leases too), set at start(). The
/// probe thread reads these to decide whether the workers are wedged — every
/// one of them busy on a statement old enough to matter — which is the only
/// condition under which it takes requests at all.
static WORKER_SLOTS: Mutex<Vec<Arc<SlotState>>> = Mutex::new(Vec::new());

pub fn set_info(base: serde_json::Value) {
    *INFO.lock().unwrap() = Some(base);
}

fn run_info(req: Request) -> (bool, u16) {
    let info = INFO.lock().unwrap().clone();
    match info {
        Some(mut v) => {
            if let Some(obj) = v.as_object_mut() {
                let up = STARTED_AT.lock().unwrap().map_or(0, |t| t.elapsed().as_millis() as u64);
                obj.insert("uptimeMs".to_string(), serde_json::Value::from(up));
                // Live clients right now — the refcount a spawned server's
                // lifetime rides on, and worth showing in any list.
                if let Some((live, _)) = connections() {
                    obj.insert("clients".to_string(), serde_json::Value::from(live));
                }
            }
            let _ = req.respond(json_response(200, &v.to_string()));
            (true, 200)
        }
        None => {
            let _ = req.respond(error_response(404, code::NOT_FOUND, "no such endpoint"));
            (true, 404)
        }
    }
}

/// One request. `exec` is the accepting thread's executor — its jobs channel
/// and cancellation slot. The probe thread passes None: it owns no
/// connection, so /catalog sheds load with an immediate 503, /ready is
/// answered from CONTROL, a session's statement is relayed to the session's
/// own connection, and every control-plane verb — session open, renew and
/// release, query cancel, /sessions, /info — works exactly as it does on a
/// worker, because none of them touch an executor.
fn handle(req: Request, exec: Option<Executor>, log: bool) -> bool {
    let path = req.url().split('?').next().unwrap_or("/").to_string();
    let method = req.method().clone();
    // Only a worker marks a slot: the probe thread owns no connection and is
    // never what `workers_wedged` is asking about.
    let _occupied = exec.map(|(_, slot)| OnRequest::enter(slot));

    // Only when logging. A clock read and a peer-address format are small, but
    // they are paid on every request by every caller, including the ones that
    // asked for a query endpoint and nothing else.
    let line = log.then(|| LogLine {
        started: Instant::now(),
        peer: req.remote_addr().map_or_else(|| "-".to_string(), |a| a.ip().to_string()),
        method: method.clone(),
        path: path.clone(),
    });
    // Cleared here so the reason logged below is this request's, never a
    // previous one's left on this worker thread.
    LAST_REASON.with(|c| c.set(""));

    // One gate before any routing: the declared length. justhttp drains an
    // undelivered body when a request is dropped — with a single
    // `vec![0; remaining]` — and it does so for EVERY response path, 404s
    // included. `take()` bounds what harbor buffers but not what the client
    // may declare, and the declared length is attacker-chosen: a request
    // declaring 1 GB and sending 9 bytes would cost a 1 GB zeroed
    // allocation. Refusing here, before anything else can respond, means the
    // allocation never happens on any path.
    //
    // Every listener is machine-local: the unix socket is protected by its
    // 0700 runtime directory and TCP binds loopback only. Callers beyond this
    // machine go through an edge proxy, where deployment policy is enforced.
    //
    // Each arm reports the status it sent, so the log line below is written in
    // one place instead of at every `respond` call. The SQL text is not
    // logged: it is the request body, it can be enormous, and on this endpoint
    // it is as likely to hold customer data as anything else in the database.
    let (keep_going, status) = if let Some(n) = req.body_length().filter(|n| *n > MAX_BODY) {
        let message = format!("body is {n} bytes; the limit is {MAX_BODY}");
        refuse(req, Refusal { status: 413, code: code::BODY_TOO_LARGE, message })
    } else if let Some(why) = from_a_browser(&req) {
        refuse(req, Refusal { status: 403, code: code::FORBIDDEN, message: why.into() })
    } else {
        match (&method, path.as_str()) {
            // Workers answer readiness down the full query path; the probe thread —
            // the one still listening when every worker is saturated —
            // answers from the CONTROL connection instead of queueing.
            (Method::Get, "/ready") => match exec {
                Some((jobs, _)) => run_ready(req, jobs),
                None => run_ready_control(req),
            },
            // Open a transaction lease: POST to the collection, REST's create.
            // It consumes a connection, which is the scarcest thing here.
            // `/new` is the route Rip's driver opens sessions at.
            (Method::Post, "/sql/sessions" | "/sql/sessions/new") => run_session_open(req),
            (Method::Post, p) if renewal_session_id(p).is_some() => {
                match lease_renew(renewal_session_id(p).unwrap()) {
                    Ok(()) => {
                        let _ = req.respond(json_response(200, r#"{"renewed":true}"#));
                        (true, 200)
                    }
                    Err(refusal) => refuse(req, refusal),
                }
            }
            // Release one. Idempotent by design: a client retrying a DELETE it
            // is not sure landed must not be able to free a connection twice.
            //
            // A session running a statement is not simply refused any more: the
            // statement is interrupted and the release completes on the
            // reaper's next tick. `released` says whether the connection is
            // back now, `cancelling` says the work to make it so is under way —
            // so a client that wants its transaction stopped has one verb for
            // it, and a client polling for the connection can tell them apart.
            (Method::Delete, p) if p.starts_with("/sql/sessions/") => {
                let released = lease_release(p.trim_start_matches("/sql/sessions/"));
                let body = wire::ReleasedResponse {
                    released: released == Released::Yes,
                    cancelling: (released == Released::Cancelling).then_some(true),
                };
                let _ = req.respond(json_response(200, &serde_json::to_string(&body).unwrap()));
                (true, 200)
            }
            // Stop a statement the client named when it sent it. Idempotent and
            // deliberately unexciting: cancelling something that already
            // finished is `false`, not an error, because by the time a Stop
            // button is pressed the query it refers to may well be over —
            // its rows may still be on their way, and they all arrive.
            (Method::Delete, p) if p.starts_with("/sql/queries/") => {
                let cancelled = cancel_query(&percent_decoded(&p["/sql/queries/".len()..]));
                let body = serde_json::to_string(&wire::CancelledResponse { cancelled }).unwrap();
                let _ = req.respond(json_response(200, &body));
                (true, 200)
            }
            // What is holding a connection, and for how long. The question an
            // operator asks when everything is suddenly waiting, and the reason
            // this exists at all: a pool you cannot see into is a pool you
            // debug by guessing. Lives at the collection the ids live under,
            // and at bare `/sessions` too.
            (Method::Get, "/sql/sessions" | "/sessions") => {
                let _ = req.respond(json_response(200, &sessions_report()));
                (true, 200)
            }
            // Fleet shutdown returns before the drain begins. Running stop()
            // on a fresh thread matters: this handler is itself one of the
            // workers stop() waits to join. POST — an action, not a resource
            // removal — and DELETE, which clients also send.
            (Method::Post | Method::Delete, "/shutdown") => {
                let _ = req.respond(json_response(202, r#"{"stopping":true}"#));
                let _ = thread::Builder::new()
                    .name("harbor-shutdown".to_string())
                    .spawn(|| {
                        let _ = stop();
                    });
                (false, 202)
            }
            // Berth identity: who serves here, which engine, since when.
            (Method::Get, "/info") => run_info(req),
            // The whole schema — tables, columns, keys, indexes, sequences — in
            // one call, in one shape. It lives here so a migration differ asks
            // a single question instead of five, and so the answer never
            // depends on which DuckDB this binary links: the queries below use
            // whatever the engine's catalog provides, and version differences
            // die in this process rather than in every client.
            (Method::Get, "/catalog") => match exec {
                Some(exec) => run_catalog(req, exec),
                None => shed(req),
            },
            (Method::Post, "/sql") => match exec {
                Some(exec) => run_sql_request(req, exec),
                // A session's statement needs no worker: it runs on the
                // session's own connection. The lane relays it, and sheds the
                // rest.
                None => return relay(req, line),
            },
            _ => refuse(req, Refusal { status: 404, code: code::NOT_FOUND, message: "no such endpoint".into() }),
        }
    };

    if let Some(line) = line {
        line.write(status);
    }
    keep_going
}

/// One request's access-log line, written once it is answered.
struct LogLine {
    started: Instant,
    peer: String,
    method: Method,
    path: String,
}

impl LogLine {
    /// After respond(), not before: justhttp writes the body from the reader
    /// inside that call, so for a streamed result the elapsed time covers the
    /// whole query and the whole transfer rather than just the headers.
    ///
    /// On a failure, name why: the refusal code turns an unexplained spike of
    /// 4xx/5xx in the berth's own log into something diagnosable. The SQL and
    /// the message stay out of the log (privacy, and the message can be
    /// large); the code is a fixed vocabulary and is enough to act on.
    fn write(self, status: u16) {
        let reason = LAST_REASON.with(|c| c.get());
        let reason = if status >= 400 && !reason.is_empty() { format!(" {reason}") } else { String::new() };
        eprintln!(
            "harbor: {} {} {} {} {status}{reason} {}ms",
            utc_now(),
            self.peer,
            self.method.as_str(),
            self.path,
            self.started.elapsed().as_millis()
        );
    }
}

/// How the probe lane runs `POST /sql`: on a thread of its own, which reads
/// the body and runs a session's statement on the session's connection, and
/// sheds anything else. The lane itself never reads a body or streams, so a
/// client that stalls either holds this thread and not the berth's last open
/// door.
///
/// Two counts bound these threads. A body is read on one of `RELAY_READERS`.
/// A statement that names a session then runs in a seat, one per lease
/// connection, which is as many statements as sessions can run at once, and
/// gives its reader back. So bodies that stall, whatever they name, hold
/// readers and never the seats sessions run in.
fn relay(req: Request, line: Option<LogLine>) -> bool {
    static READERS: AtomicUsize = AtomicUsize::new(0);
    static SEATS: AtomicUsize = AtomicUsize::new(0);
    let log = |line: Option<LogLine>, (keep_going, status): (bool, u16)| {
        if let Some(line) = line {
            line.write(status);
        }
        keep_going
    };
    let Some(reading) = Seat::take(&READERS, RELAY_READERS) else {
        return log(line, shed(req));
    };
    // A thread that cannot start drops the request, which justhttp answers
    // with a 500, and the reader with it.
    let _ = thread::Builder::new().name("harbor-relay".to_string()).spawn(move || {
        let mut req = req;
        let answer = match read_request_body(&mut req)
            .and_then(|body| parse_request(&body).map_err(Refusal::bad_request))
        {
            Err(refusal) => refuse(req, refusal),
            Ok(parsed) if parsed.session.is_none() => shed(req),
            Ok(parsed) => {
                let cap = LEASES.lock().unwrap().as_ref().map_or(0, |l| l.total);
                match Seat::take(&SEATS, cap) {
                    Some(_seat) => {
                        drop(reading);
                        run_sql(req, parsed, None)
                    }
                    None => shed(req),
                }
            }
        };
        log(line, answer);
    });
    true
}

/// How many relay threads may read a body at once. A body lands in
/// milliseconds, so a few are plenty, and the count keeps a client that
/// pipelines requests from turning each into a thread.
const RELAY_READERS: usize = 8;

/// One of a bounded count of relay threads, given back when dropped.
struct Seat(&'static AtomicUsize);

impl Seat {
    /// A seat when fewer than `cap` are taken. Built before the count is
    /// raised, so every path, a refusal included, lowers it again.
    fn take(count: &'static AtomicUsize, cap: usize) -> Option<Seat> {
        let seat = Seat(count);
        (count.fetch_add(1, Ordering::SeqCst) < cap).then_some(seat)
    }
}

impl Drop for Seat {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Why a TCP request came from a web page, or `None` when it did not.
///
/// Loopback is exactly where a browser on the same machine sends a page's
/// requests. A page can POST a text/plain body without a CORS preflight, and
/// DNS rebinding lets it read the answers under a hostname it controls. Every
/// browser request that carries a body names its page in `Origin`, and a
/// rebound one names the page's hostname in `Host`. Harbor's own clients send
/// no `Origin`, and their `Host` is the address they dialled, which passes
/// whenever that is `localhost` or an IP address. The unix socket is out of a browser's reach and skips both checks.
/// A browser client behind an edge proxy is that proxy's policy: it drops
/// `Origin` and sends the upstream's own `Host`.
fn from_a_browser(req: &Request) -> Option<&'static str> {
    fn header<'r>(req: &'r Request, name: &'static str) -> Option<&'r str> {
        req.headers().iter().find(|h| h.field.equiv(name)).map(|h| h.value.as_str())
    }
    req.remote_addr()?;
    if header(req, "Origin").is_some() {
        return Some("a web page may not reach harbor: the request carries an Origin");
    }
    match header(req, "Host") {
        Some(host) if !loopback_host(host) => {
            Some("Host must be localhost or an IP address, not a hostname a web page controls")
        }
        _ => None,
    }
}

/// `localhost` or an IP literal, with or without a port. A hostname is what
/// DNS rebinding needs; an address cannot be rebound.
fn loopback_host(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(bracketed) => bracketed.split(']').next().unwrap_or(""),
        None => host.rsplit_once(':').map_or(host, |(name, _)| name),
    };
    name.eq_ignore_ascii_case("localhost") || name.parse::<std::net::IpAddr>().is_ok()
}

/// Marks this worker's slot occupied for the life of one request, so the
/// probe thread can tell a worker that is stuck from one that is free even
/// before a statement exists (see `workers_wedged`).
///
/// Same RAII discipline as `Claim`/`Cancellable`/`OnSlot`: cleared on every
/// path out of `handle`, panic included, because a slot left marked occupied
/// would make the probe thread believe a free worker was wedged forever.
struct OnRequest<'a> {
    slot: &'a Arc<SlotState>,
}

impl<'a> OnRequest<'a> {
    fn enter(slot: &'a Arc<SlotState>) -> Self {
        slot.run.lock().unwrap().wedged_at = Some(Instant::now() + WEDGED_REQUEST_AGE);
        OnRequest { slot }
    }
}

impl Drop for OnRequest<'_> {
    fn drop(&mut self) {
        self.slot.run.lock().unwrap().wedged_at = None;
    }
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


/// The route list, method included — a test-checked mirror of the dispatch
/// match in `handle`, kept so the wire crate's published endpoints and what
/// this server actually answers can never drift apart silently. GET /sql is
/// not a route (the method matters).
#[cfg(test)]
fn route_exists(method: &Method, path: &str) -> bool {
    matches!(
        (method, path),
        (Method::Get, "/ready" | "/sql/sessions" | "/sessions" | "/info" | "/catalog")
            | (Method::Post | Method::Delete, "/shutdown")
            | (Method::Post, "/sql" | "/sql/sessions" | "/sql/sessions/new")
    ) || (*method == Method::Post && renewal_session_id(path).is_some())
      || (*method == Method::Delete
        && (path.starts_with("/sql/sessions/") || path.starts_with("/sql/queries/")))
}

/// The request body: one statement, optional positional parameters.
struct SqlRequest {
    sql: String,
    params: Vec<Param>,
    /// Names a lease. Absent means the statement runs on a worker connection
    /// and settles itself, which is what almost every request wants.
    session: Option<String>,
    /// A name the client chose so it can cancel this statement later. Chosen by
    /// the client rather than minted here because the alternative — answering
    /// with an id — cannot work: the response does not begin until the
    /// statement is finished or streaming, and by then the id is no use.
    query: Option<String>,
    /// How long this statement may run. Overrides the deployment default in
    /// either direction, including downward from unlimited.
    timeout: Option<Duration>,
}

fn parse_request(body: &str) -> Result<SqlRequest, String> {
    /// The body in one pass. `params` stays as written, so each param's own
    /// text can say whether it is a whole number, and a key given twice is
    /// refused rather than read as one of its values.
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase", expecting = "an object")]
    struct Body<'a> {
        sql: Option<serde_json::Value>,
        #[serde(borrow)]
        params: Option<&'a serde_json::value::RawValue>,
        session_id: Option<serde_json::Value>,
        query_id: Option<serde_json::Value>,
        timeout_ms: Option<serde_json::Value>,
    }
    let v: Body = serde_json::from_str(body).map_err(json_error)?;
    // The derive also reads an array of the fields in order, which is not a
    // request.
    let sql = match v.sql {
        Some(serde_json::Value::String(s)) if body.trim_start().starts_with('{') => s,
        _ => return Err("missing \"sql\"".to_string()),
    };
    if sql.trim().is_empty() {
        return Err("\"sql\" is empty".to_string());
    }
    let params = match v.params {
        None => Vec::new(),
        Some(raw) => serde_json::from_str::<Vec<&serde_json::value::RawValue>>(raw.get())
            .map_err(|_| "\"params\" must be an array".to_string())?
            .into_iter()
            .map(|raw| {
                let param = serde_json::from_str(raw.get()).map_err(json_error)?;
                json_to_duckdb(param, || !raw.get().contains(['.', 'e', 'E']))
            })
            .collect::<Result<_, _>>()?,
    };
    let session = match v.session_id {
        None => None,
        Some(serde_json::Value::String(id)) if !id.is_empty() => Some(id),
        Some(_) => return Err("\"sessionId\" must be a non-empty string".to_string()),
    };
    let query = match v.query_id {
        None => None,
        // Bounded, because it becomes a key in a map that lives as long as the
        // server and is written by any caller.
        Some(serde_json::Value::String(id)) if !id.is_empty() && id.len() <= 128 => Some(id),
        Some(_) => {
            return Err("\"queryId\" must be a non-empty string of at most 128 characters"
                .to_string());
        }
    };
    // The operator's `--statement-timeout` is a hard ceiling, not just a
    // default: a request may ask for *less*, but not for more, and `0` ("no
    // limit") is bounded by it too. Without the clamp, any caller could
    // send `timeoutMs:0` and pin a worker indefinitely — defeating the very
    // knob a `--sealed` deployment leans on. With no cap configured, 0 is
    // unlimited and N is exactly N.
    let cap = configured_statement_timeout();
    let timeout = match v.timeout_ms {
        None => cap,
        Some(serde_json::Value::Number(n)) => match n.as_u64() {
            Some(0) => cap,
            Some(ms) => {
                let want = Duration::from_millis(ms);
                Some(cap.map_or(want, |c| want.min(c)))
            }
            None => return Err("\"timeoutMs\" must be a non-negative whole number".to_string()),
        },
        Some(_) => return Err("\"timeoutMs\" must be a non-negative whole number".to_string()),
    };
    Ok(SqlRequest { sql, params, session, query, timeout })
}

/// One JSON param as the value it binds. `whole` says whether the param was
/// written as a whole number, with no fraction and no exponent: one past 64
/// bits parses to a double, so only its text can say. It is asked only of a
/// number past what 64 bits hold.
fn json_to_duckdb(v: serde_json::Value, whole: impl FnOnce() -> bool) -> Result<Param, String> {
    Ok(match v {
        serde_json::Value::Null => Param::Null,
        serde_json::Value::Bool(b) => Param::Bool(b),
        serde_json::Value::String(s) => Param::Text(s),
        // A fraction or an exponent binds as the double its text names, as
        // in SQL. A whole number past i64 and u64 reads as the nearest
        // double, a different number, so it is refused rather than stored
        // as one: JSON numbers do not carry it exactly through most clients
        // either, and a string cast in the statement does.
        serde_json::Value::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => Param::I64(i),
            (_, Some(u), _) => Param::U64(u),
            (_, _, Some(f)) if f.fract() == 0.0 && f.abs() >= 9_223_372_036_854_775_808.0 && whole() => {
                return Err("a whole number param past what 64 bits hold would bind as a different \
                            number: send it as a string and cast it in the statement, as ?::HUGEINT"
                    .to_string());
            }
            (_, _, Some(f)) => Param::F64(f),
            _ => return Err("unrepresentable number in \"params\"".to_string()),
        },
        // An object or an array is a document. Aimed at a VARIANT it is bound
        // as one (`Conn::bind`); everywhere else it has no SQL type of
        // its own and goes as its JSON text, for the statement to cast. A
        // string is never read this way, whatever it spells: a param that
        // looks like JSON is data, as one that looks like SQL is.
        other if nests_within(&other, DOCUMENT_LEVELS) => {
            Param::Document { text: other.to_string(), variant: false }
        }
        _ => return Err(too_deep()),
    })
}

/// A JSON parse failure in words. The parser stops reading at 127 levels,
/// and nothing in a request nests but a document param, so its refusal is
/// the one below in other words. Should the wording ever differ, the
/// parser's own message goes out instead.
fn json_error(e: serde_json::Error) -> String {
    match e.to_string() {
        deep if deep.starts_with("recursion limit exceeded") => too_deep(),
        other => other,
    }
}

fn too_deep() -> String {
    format!("a document param nests at most {DOCUMENT_LEVELS} levels")
}

/// How deep an object or array param may nest, counting its own levels: `{}`
/// is one, `[[1]]` is two. Rip's ORM and DuckTable's editor keep the same
/// number, so a document is refused at the same depth whichever layer meets it
/// first. The engine is why there is a number at all: an `UPDATE` of a VARIANT
/// column costs the square of the nesting depth (duckdb#25967).
const DOCUMENT_LEVELS: usize = 100;

/// Whether `v` nests no deeper than `levels`. It descends `levels` deep and
/// stops, so its own recursion is bounded by the limit it checks and not by
/// the document.
fn nests_within(v: &serde_json::Value, levels: usize) -> bool {
    match v {
        serde_json::Value::Array(a) => levels > 0 && a.iter().all(|c| nests_within(c, levels - 1)),
        serde_json::Value::Object(o) => levels > 0 && o.values().all(|c| nests_within(c, levels - 1)),
        _ => true,
    }
}

/// Which shape the caller asked for. NDJSON is the default and the only one
/// that streams; see `wants_one_shot`.
///
/// The two are not different encodings of a result — the column schema and
/// every value are produced by exactly the same code — only different framing
/// around it. That is deliberate: a second encoder is a second thing to keep
/// correct, and the values are the part that is hard.
#[derive(Clone, Copy, PartialEq)]
enum Shape {
    Ndjson,
    Json,
}

/// `Accept: application/json` asks for the whole result as one document.
/// Anything else — no header, `*/*`, `application/x-ndjson`
/// — streams.
///
/// A header naming both wins for NDJSON: it is the shape that cannot fail on
/// size, so it is the safe reading of an ambiguous request. (`application/json`
/// is not a substring of `application/x-ndjson`, so a plain `contains` is not
/// fooled by the streaming type.)
fn wants_one_shot(req: &Request) -> bool {
    // case-insensitive substring scan, allocation-free
    fn contains_ignore_case(hay: &str, needle: &str) -> bool {
        hay.as_bytes()
            .windows(needle.len())
            .any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
    }
    let mut asked_json = false;
    for h in req.headers().iter().filter(|h| h.field.equiv("Accept")) {
        let value = h.value.as_str();
        if contains_ignore_case(value, "application/x-ndjson") {
            return false;
        }
        asked_json = asked_json || contains_ignore_case(value, "application/json");
    }
    asked_json
}

/// Why a job could not produce a result, and how to say so over HTTP.
///
/// The code travels with the message because the two are not derivable from
/// each other: a result too large for the shape the caller asked for is not a
/// SQL error, and a client that classifies on `code` — as rip/db does, to tell
/// a bad query apart from a server it should retry — has to be told which it
/// was.
struct Refusal {
    status: u16,
    code: &'static str,
    message: String,
}

impl Refusal {
    /// The engine rejected the statement. By far the common case, so it gets
    /// the short spelling.
    fn sql(message: impl Into<String>) -> Self {
        Self { status: 400, code: code::SQL_ERROR, message: message.into() }
    }

    /// The request itself is malformed.
    fn bad_request(message: impl Into<String>) -> Self {
        Self { status: 400, code: code::BAD_REQUEST, message: message.into() }
    }

    fn not_serving() -> Self {
        Self { status: 503, code: code::UNAVAILABLE, message: "harbor is not serving".into() }
    }

    /// The same answer whether the session never existed, was released, or
    /// timed out: from the client's side those are one situation — the
    /// transaction is gone and the work has to start again.
    fn no_session() -> Self {
        Self {
            status: 404,
            code: code::NO_SUCH_SESSION,
            message: "no such session: it was released, timed out, or never existed. Open a new \
                      one and retry the transaction from the beginning."
                .into(),
        }
    }

    /// Somebody stopped this statement on purpose.
    ///
    /// 499 is nginx's, not the RFC's, and it is the right borrow: there is no
    /// standard code for "the caller withdrew", 400 would blame the statement
    /// and 500 would blame harbor, when in fact nothing went wrong. Clients
    /// here branch on `code` rather than status anyway — rip/db keeps
    /// `harborCode` separate for exactly that — so the status is for logs and
    /// proxies, and the code is the interface.
    fn cancelled() -> Self {
        Self {
            status: 499,
            code: code::CANCELLED,
            message: "this statement was cancelled before it finished".to_string(),
        }
    }
}

/// A DuckDB error is a cancellation when harbor asked for one, and an engine
/// error otherwise. Decided from the slot's flag rather than by matching
/// "INTERRUPT" in the message, because an error string is prose and can be
/// reworded upstream without warning; the flag is harbor's own record of
/// having fired the interrupt.
fn refusal_for(cancelled: bool, message: String) -> Refusal {
    match cancelled {
        true => Refusal::cancelled(),
        false => Refusal::sql(message),
    }
}

/// One unit of work for an executor thread.
struct Job {
    sql: String,
    params: Vec<Param>,
    shape: Shape,
    /// Process-unique, assigned before the job is sent, so a cancel arriving
    /// from another thread can name this statement and no other.
    id: u64,
    /// When to stop trying, if anything asked for a limit.
    deadline: Option<Instant>,
    /// Return this connection to a clean state instead of running `sql`.
    ///
    /// Not the same as sending `ROLLBACK` as a statement, which would go
    /// through prepare, query and row iteration and then fail on the common
    /// path — a lease released after COMMIT has nothing to roll back, so every
    /// release would manufacture an error and a result nobody reads. This is
    /// the call the workers already make between requests.
    reset: bool,
    /// Answered exactly once, before any body byte is produced. `Err` means
    /// nothing has been written yet, so the worker can still pick a status
    /// code — which is the whole reason preparation is reported separately
    /// from streaming.
    ready: mpsc::SyncSender<Result<(), Refusal>>,
    /// Body bytes, in envelope-line batches. Bounded, so a slow client
    /// applies backpressure to the query instead of buffering the result.
    body: mpsc::SyncSender<Vec<u8>>,
}

impl Job {
    /// A job with a fresh id, and the ends its executor answers on.
    fn new(
        sql: String,
        params: Vec<Param>,
        shape: Shape,
        timeout: Option<Duration>,
    ) -> (Job, mpsc::Receiver<Result<(), Refusal>>, mpsc::Receiver<Vec<u8>>) {
        let (ready, ready_rx) = mpsc::sync_channel(1);
        let (body, body_rx) = mpsc::sync_channel(BODY_QUEUE);
        let deadline = timeout.map(|t| Instant::now() + t);
        let job = Job { sql, params, shape, id: next_job_id(), deadline, reset: false, ready, body };
        (job, ready_rx, body_rx)
    }
}

/// How many body batches may be in flight before the query has to wait.
const BODY_QUEUE: usize = 4;

/// How long a `/ready` verdict is served before another query is run to
/// refresh it.
///
/// Readiness has to run a real query to mean anything, so without a cache a
/// busy prober can make the server work on the same bounded pool that serves
/// query traffic. One second bounds that to one query per second no matter how often
/// it is asked, which is well inside what any prober polls at. The cost is that
/// a database that wedges is reported ready for up to a second longer; a probe
/// interval is measured in seconds, so nothing observes the difference.
const READY_MAX_AGE: Duration = Duration::from_secs(1);

/// The last verdict and when it was taken. Failures are cached too — a server
/// that cannot answer is exactly the one that must not be asked N more times a
/// second.
static LAST_READY: Mutex<Option<(Instant, bool)>> = Mutex::new(None);

/// Read one extra byte to distinguish a complete body from a valid prefix
/// of an oversized chunked request. Decode only after enforcing the byte cap.
fn read_request_body(req: &mut Request) -> Result<String, Refusal> {
    let capacity = req.body_length().unwrap_or(0).min(16 * 1024);
    read_body(req.as_reader(), capacity)
}

fn read_body(reader: impl Read, capacity: usize) -> Result<String, Refusal> {
    let mut body = Vec::with_capacity(capacity);
    reader.take(MAX_BODY as u64 + 1).read_to_end(&mut body)
        .map_err(|e| Refusal::bad_request(format!("the request body could not be read: {e}")))?;
    if body.len() > MAX_BODY {
        return Err(Refusal {
            status: 413,
            code: code::BODY_TOO_LARGE,
            message: format!("body exceeds the limit of {MAX_BODY} bytes"),
        });
    }
    String::from_utf8(body).map_err(|_| Refusal::bad_request("the request body is not valid UTF-8"))
}

/// `POST /sql/sessions` — take a connection out of the pool and hold it.
///
/// Interactive leases have a capped lifetime and idle timeout. A backup lease
/// instead has a renewable liveness window, confirmed by `purpose` in the reply.
fn run_session_open(mut req: Request) -> (bool, u16) {
    let body = match read_request_body(&mut req) {
        Ok(body) => body,
        Err(refusal) => return refuse(req, refusal),
    };
    let requested = if body.trim().is_empty() {
        wire::SessionNewRequest::default()
    } else {
        match serde_json::from_str::<wire::SessionNewRequest>(&body) {
            Ok(v) => v,
            Err(e) => return refuse(req, Refusal::bad_request(e.to_string())),
        }
    };
    if requested.ttl_ms == Some(0) {
        return refuse(req, Refusal::bad_request("\"ttlMs\" must be a positive integer"));
    }
    let backup = requested.purpose == Some(wire::SessionPurpose::Backup);

    match lease_open(requested.ttl_ms.map(Duration::from_millis), backup) {
        Ok((id, ttl, idle_ttl)) => {
            let body = serde_json::to_string(&wire::SessionNewResponse {
                session_id: id,
                ttl_ms: ttl.as_millis() as u64,
                idle_ttl_ms: idle_ttl.as_millis() as u64,
                purpose: requested.purpose,
            }).expect("session response serializes");
            let _ = req.respond(json_response(200, &body));
            (true, 200)
        }
        // Exhaustion is temporary by definition — a lease is held for the
        // length of a transaction, not a session — so say how long to wait
        // instead of leaving the client to invent a backoff. ActiveRecord
        // raises ConnectionTimeoutError and tells you nothing; this is the
        // same situation with the one useful number attached.
        Err(refusal) => {
            let mut response = error_response(refusal.status, refusal.code, &refusal.message);
            if refusal.code == code::NO_LEASE_AVAILABLE {
                response.add_header(
                    Header::from_bytes(&b"Retry-After"[..], &b"1"[..]).unwrap(),
                );
            }
            let _ = req.respond(response);
            (true, refusal.status)
        }
    }
}

/// `GET /sessions` — every lease, and the accounting behind them.
///
/// `connections` is the conservation invariant made visible: free plus live
/// plus inflight always equals total. `balanced` is that equality checked at
/// the moment of the request. It is not decoration — a pool leaks connections
/// silently and the symptom arrives weeks later as "everything hangs", so the
/// arithmetic that would have caught it is worth being able to read.
fn sessions_report() -> String {
    let guard = LEASES.lock().unwrap();
    let Some(leases) = guard.as_ref() else {
        return r#"{"serving":false,"sessions":[]}"#.to_string();
    };
    let now = Instant::now();
    let mut out = String::from("{\"serving\":true,\"connections\":{");
    out.push_str(&format!(
        r#""total":{},"free":{},"live":{},"inflight":{},"balanced":{}"#,
        leases.total,
        leases.free.len(),
        leases.live.len(),
        leases.inflight,
        leases.accounted() == leases.total
    ));
    out.push_str("},\"sessions\":[");
    let mut sessions: Vec<(&String, &Lease)> = leases.live.iter().collect();
    // Oldest first: the one that has been holding a connection longest is the
    // one being looked for.
    sessions.sort_by_key(|(_, l)| l.opened);
    for (i, (id, lease)) in sessions.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            r#"{{"sessionId":"{}","slot":{},"ageMs":{},"idleMs":{},"expiresInMs":{},"statements":{},"inTransaction":{},"busy":{},"renewable":{}}}"#,
            id,
            lease.conn.slot,
            now.duration_since(lease.opened).as_millis(),
            now.duration_since(lease.last).as_millis(),
            lease.lifetime.deadline.saturating_duration_since(now).as_millis(),
            lease.statements,
            lease.in_transaction,
            lease.busy,
            lease.lifetime.renewal_ttl.is_some()
        ));
    }
    out.push_str("]}");
    out
}

// ---------------------------------------------------------------------------
// GET /catalog
// ---------------------------------------------------------------------------

/// Why a catalog query could not answer. `Gone` is the executor being dead —
/// the one condition the worker must act on (leave its accept loop, exactly
/// as `run_sql` does) rather than merely report.
enum CatalogFailure {
    Refused(Refusal),
    Gone,
}

/// Run one catalog query on this worker's own executor — the same connection
/// and the same discipline as `/sql`, one bounded statement at a time — and
/// hand back the rows parsed rather than streamed. The one-shot JSON shape is
/// reused instead of a second reader being written: the executor already
/// produces `{"ok":true,...,"data":[...]}`, and a catalog result is a few
/// dozen rows, nowhere near the size that shape refuses. Watched for the
/// client as a statement is, so a client that leaves, as DuckTable does
/// when it moves to another database, stops the exact count over every
/// table with it.
fn catalog_rows(
    (jobs, slot): Executor,
    peer: &justhttp::Peer,
    sql: &str,
) -> Result<Vec<Vec<serde_json::Value>>, CatalogFailure> {
    // The deployment default applies here as it does to any statement.
    let (job, ready_rx, body_rx) = Job::new(sql.to_string(), Vec::new(), Shape::Json, configured_statement_timeout());
    let id = job.id;
    if jobs.send(job).is_err() {
        return Err(CatalogFailure::Gone);
    }
    let mut watch = Watch::new(Some(peer.clone()), Arc::clone(slot), id);
    let verdict = watch.recv(&ready_rx);
    // Drain rather than drop, for the same reason `run_ready` does: a dropped
    // receiver reads as a client that hung up mid-stream and costs a rollback.
    let mut document = Vec::new();
    while let Ok(chunk) = watch.recv(&body_rx) {
        document.extend_from_slice(&chunk);
    }
    match verdict {
        Ok(Ok(())) => {}
        Ok(Err(refusal)) => return Err(CatalogFailure::Refused(refusal)),
        Err(_) => return Err(CatalogFailure::Gone),
    }
    let doc: serde_json::Value = match serde_json::from_slice(&document) {
        Ok(doc) => doc,
        Err(e) => {
            return Err(CatalogFailure::Refused(Refusal {
                status: 500,
                code: code::INTERNAL,
                message: format!("a catalog result did not parse: {e}"),
            }));
        }
    };
    let rows = doc.get("data").and_then(|d| d.as_array()).cloned().unwrap_or_default();
    Ok(rows.into_iter().map(|r| r.as_array().cloned().unwrap_or_default()).collect())
}

/// The failure paths every catalog query shares.
fn catalog_refuse(req: Request, failure: CatalogFailure) -> (bool, u16) {
    match failure {
        CatalogFailure::Refused(r) => {
            let _ = req.respond(error_response(r.status, r.code, &r.message));
            (true, r.status)
        }
        CatalogFailure::Gone => {
            let _ = req.respond(error_response(503, code::UNAVAILABLE, "harbor is shutting down"));
            (false, 503)
        }
    }
}

// One cell out of a catalog row, by position. The queries below name their
// columns, so a position is stable; a cell of the wrong type answers the
// empty value rather than panicking on data a future engine might put there.

fn cell_str(row: &[serde_json::Value], i: usize) -> String {
    row.get(i).and_then(|v| v.as_str()).unwrap_or_default().to_string()
}

fn cell_opt_str(row: &[serde_json::Value], i: usize) -> Option<String> {
    row.get(i).and_then(|v| v.as_str()).map(str::to_string)
}

fn cell_bool(row: &[serde_json::Value], i: usize) -> bool {
    row.get(i).and_then(|v| v.as_bool()).unwrap_or(false)
}

fn cell_opt_u64(row: &[serde_json::Value], i: usize) -> Option<u64> {
    // The executor's integer policy quotes a value past JSON's exact range,
    // so a cell can arrive as either a number or its decimal string.
    row.get(i)
        .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
}

fn cell_list(row: &[serde_json::Value], i: usize) -> Vec<String> {
    row.get(i)
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).map(str::to_string).collect())
        .unwrap_or_default()
}

/// One entry of an index's column list: a plain column, or a computed
/// expression. They arrive rendered the same way and mean different things,
/// so the contract keeps them apart rather than making every client guess.
enum IndexPart {
    Column(String),
    Expression(String),
}

/// The column list of an index, recovered from duckdb_indexes()'
/// `expressions` field.
///
/// Neither engine harbor targets exposes the list structurally: the field is
/// the VARCHAR rendering of a LIST — `[email]`, `[title, user_id]`,
/// `['(lower("name"))']` — so this undoes exactly that rendering. Items are
/// comma-separated; an item that is anything beyond a plain identifier is
/// single-quoted with `\'` and `\\` escapes. This is DuckDB's own
/// machine-generated list syntax with fixed quoting rules, not prose, so
/// undoing it is exact.
fn index_parts(expressions: &str) -> Vec<IndexPart> {
    index_columns(expressions)
        .into_iter()
        .map(|item| -> IndexPart {
            // DuckDB single-quotes any item that is not a bare identifier,
            // which covers two different things: an identifier that needed
            // double-quoting (`"a b"`, `"é"`) and a real expression
            // (`(lower("name"))`). Only the first is a column name, and
            // leaving its quotes on is what made `indexes[].columns` fail to
            // join against `columns[].name` — three of five names on an
            // ordinary table. Undo the quoting here, once, instead of asking
            // every client to reimplement it; anything that is not a
            // well-formed quoted identifier is an expression and is labelled
            // as one.
            if let Some(name) = unquote_identifier(&item) {
                return IndexPart::Column(name);
            }
            // Rendered bare, which DuckDB only does for a name that needs no
            // quoting at all — so a run of identifier characters is a column,
            // and anything carrying a paren, an operator, a space or a quote
            // is an expression.
            let bare = !item.is_empty()
                && !item.starts_with(|c: char| c.is_ascii_digit())
                && item.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$');
            match bare {
                true => IndexPart::Column(item),
                false => IndexPart::Expression(item),
            }
        })
        .collect()
}

/// `"a b"` -> `a b`, undoubling `""`. None when the text is not exactly one
/// double-quoted identifier — an expression, or a bare word that needs no
/// undoing (the caller keeps those as-is via `Column` below).
fn unquote_identifier(item: &str) -> Option<String> {
    let inner = item.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '"' {
            out.push(c);
            continue;
        }
        // A lone `"` inside would have closed the identifier, so the only
        // legal appearance is a doubled pair.
        match chars.next() {
            Some('"') => out.push('"'),
            _ => return None,
        }
    }
    Some(out)
}

fn index_columns(expressions: &str) -> Vec<String> {
    let trimmed = expressions.trim();
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(trimmed);
    let mut items = Vec::new();
    let mut chars = inner.chars().peekable();
    loop {
        while matches!(chars.peek(), Some(' ') | Some(',')) {
            chars.next();
        }
        let Some(&c) = chars.peek() else { break };
        let mut item = String::new();
        if c == '\'' {
            chars.next();
            while let Some(ch) = chars.next() {
                match ch {
                    '\\' => {
                        if let Some(escaped) = chars.next() {
                            item.push(escaped);
                        }
                    }
                    '\'' => break,
                    _ => item.push(ch),
                }
            }
        } else {
            while let Some(&ch) = chars.peek() {
                if ch == ',' {
                    break;
                }
                item.push(ch);
                chars.next();
            }
            while item.ends_with(' ') {
                item.pop();
            }
        }
        items.push(item);
    }
    items
}

/// The served file's actual bytes on disk, from the one process that can
/// stat them. `(data, wal)` — a checkpointed database legitimately has no
/// WAL file, which is 0 bytes of WAL, not an unknown. A berth serving no
/// file (or one whose path stopped answering) reports neither, and the
/// engine's pretty-printed sizes ("1.2 MiB") are never in the contract:
/// clients render their own units from exact bytes or from nothing.
fn database_disk_sizes() -> (Option<u64>, Option<u64>) {
    let info = INFO.lock().unwrap();
    let Some(path) = info.as_ref().and_then(|v| v.get("database")).and_then(|v| v.as_str())
    else {
        return (None, None);
    };
    let Ok(data) = std::fs::metadata(path) else { return (None, None) };
    let wal = std::fs::metadata(format!("{path}.wal")).map(|m| m.len()).unwrap_or(0);
    (Some(data.len()), Some(wal))
}

/// Which fidelity `/catalog` answers at. Lite is the inventory — what
/// exists; full adds how everything is built and its exact row counts.
#[derive(Clone, Copy)]
enum CatalogStyle {
    Full,
    Lite,
}

/// `?style=` from the request url. No query and no `style` mean full. An
/// unknown *value* is refused loudly — a style the caller asked for and did
/// not get would corrupt silently — while unknown *parameters* pass, so a
/// client may send one a server does not know and still get a correct answer.
fn catalog_style(url: &str) -> Result<CatalogStyle, String> {
    let Some(query) = url.split_once('?').map(|x| x.1) else { return Ok(CatalogStyle::Full) };
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == "style" {
            return match value {
                "full" => Ok(CatalogStyle::Full),
                "lite" => Ok(CatalogStyle::Lite),
                other => Err(format!(
                    "unknown catalog style {other:?} — this harbor answers full (the default) and lite"
                )),
            };
        }
    }
    Ok(CatalogStyle::Full)
}

/// Build the one exact-count statement for a catalog inventory. The ordinal
/// travels through the result and is sorted explicitly: UNION ALL preserves
/// duplicates, not branch order, and the caller must never attach a count to
/// the wrong table. Identifiers come only from DuckDB's catalog and are still
/// quoted as SQL identifiers, including embedded double quotes.
fn catalog_count_sql(table_rows: &[Vec<serde_json::Value>]) -> Option<String> {
    if table_rows.is_empty() {
        return None;
    }
    let mut sql = String::from("SELECT table_ordinal, row_count FROM (");
    for (i, row) in table_rows.iter().enumerate() {
        if i > 0 {
            sql.push_str(" UNION ALL ");
        }
        let schema = catalog_identifier(&cell_str(row, 0));
        let table = catalog_identifier(&cell_str(row, 1));
        let _ = write!(
            sql,
            "SELECT {i}::UBIGINT AS table_ordinal, count(*)::UBIGINT AS row_count FROM {schema}.{table}"
        );
    }
    sql.push_str(") AS exact_counts ORDER BY table_ordinal");
    Some(sql)
}

fn catalog_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `GET /catalog` — the complete schema shape a migration differ needs, as
/// one stable JSON contract.
///
/// The contract is the deliverable. The queries below read whatever catalog
/// functions the linked engine provides; what goes out never varies with the
/// engine version, so a client diffing two schemas never has to know which
/// DuckDB produced either of them. Every query names its columns — an engine
/// that dropped one fails loudly here rather than silently shifting positions.
///
/// Foreign keys come from duckdb_constraints()' structured fields —
/// constraint_column_names, referenced_table, referenced_column_names —
/// never from parsing constraint_text prose; ending that parsing in clients
/// is the reason this endpoint exists. The referenced schema is not a catalog
/// field, and does not need to be one: DuckDB refuses to create a foreign key
/// across schemas or catalogs, so the referenced table's schema is the
/// referencing table's own.
///
/// Unique constraints come from the same structured fields: an inline
/// `UNIQUE` on a column and a table-level `UNIQUE (a, b)` both arrive as
/// `constraint_type = 'UNIQUE'` rows with constraint_column_names in
/// declaration order. They are the uniqueness `duckdb_indexes()` cannot show
/// — its list holds only what CREATE INDEX made — so without them a
/// hand-written `CREATE TABLE (... UNIQUE)` schema reads as having no
/// uniqueness at all. PRIMARY KEY is its own constraint type and its own
/// field, and never appears here.
///
/// The document also carries what a browsing client otherwise dials more
/// queries for: each table's exact `rowCount`, each table's `ddl` as the
/// engine renders it, and `databaseSizeBytes`/`walSizeBytes` statted from the
/// served file by the one process sitting next to it — exact bytes, never
/// the engine's pretty-printed strings, and null for a berth serving no file.
///
/// `?style=lite` answers the inventory alone: the versions, the sizes, and
/// each table's name and schema — enough to draw a database list without
/// paying for counts, columns, constraints, indexes, DDL, or sequences, in
/// queries here or in bytes on the wire. It is the same document family at
/// lower fidelity, not a second contract: a field a style omits is absent,
/// never differently shaped.
///
/// Ordering is part of the contract: tables by (schema, name), columns in
/// ordinal position, indexes and sequences by name, unique constraints by
/// their column lists, foreign keys by their referenced table and column
/// lists. A stable database answers with byte-identical output.
fn run_catalog(req: Request, exec: Executor) -> (bool, u16) {
    let peer = req.peer();
    let style = match catalog_style(req.url()) {
        Ok(style) => style,
        Err(message) => {
            let _ = req.respond(error_response(400, code::BAD_REQUEST, &message));
            return (true, 400);
        }
    };
    // System and temp catalogs are excluded by anchoring every query to the
    // served database: `system` and `temp` are separate databases, so
    // current_database() never matches them.
    let version_rows = match catalog_rows(exec, &peer, "SELECT library_version FROM pragma_version()") {
        Ok(rows) => rows,
        Err(failure) => return catalog_refuse(req, failure),
    };
    let table_sql = match style {
        CatalogStyle::Full => {
            "SELECT schema_name, table_name, sql FROM duckdb_tables() \
             WHERE database_name = current_database() AND NOT internal AND NOT temporary \
             ORDER BY schema_name, table_name"
        }
        CatalogStyle::Lite => {
            "SELECT schema_name, table_name FROM duckdb_tables() \
             WHERE database_name = current_database() AND NOT internal AND NOT temporary \
             ORDER BY schema_name, table_name"
        }
    };
    let table_rows = match catalog_rows(exec, &peer, table_sql) {
        Ok(rows) => rows,
        Err(failure) => return catalog_refuse(req, failure),
    };
    let duckdb_version = version_rows.first().map(|r| cell_str(r, 0)).unwrap_or_default();
    let harbor_version = env!("CARGO_PKG_VERSION").to_string();
    let (database_size_bytes, wal_size_bytes) = database_disk_sizes();

    // The lite style stops here: everything it answers is already in hand,
    // and the count plus four shape queries below never run.
    if let CatalogStyle::Lite = style {
        let tables = table_rows.iter().map(|row| Named { name: cell_str(row, 1), schema: cell_str(row, 0) });
        let inventory = Inventory {
            harbor_version,
            duckdb_version,
            database_size_bytes,
            wal_size_bytes,
            tables: tables.collect(),
        };
        let _ = req.respond(json_response(200, &serde_json::to_string(&inventory).unwrap()));
        return (true, 200);
    }

    // SQL cannot turn values returned by duckdb_tables() into relation
    // identifiers. Build those identifiers here, where they can be quoted
    // exactly, then let one UNION ALL statement count every table under one
    // query snapshot. COUNT(*) projects no application columns; it reads the
    // engine's visibility information and therefore excludes deleted rows
    // that the physical storage cardinality still includes.
    let count_rows = if let Some(sql) = catalog_count_sql(&table_rows) {
        match catalog_rows(exec, &peer, &sql) {
            Ok(rows) => rows,
            Err(failure) => return catalog_refuse(req, failure),
        }
    } else {
        Vec::new()
    };
    let row_counts = if count_rows.len() == table_rows.len() {
        count_rows
            .iter()
            .enumerate()
            .map(|(i, row)| {
                if cell_opt_u64(row, 0) != Some(i as u64) {
                    return None;
                }
                cell_opt_u64(row, 1)
            })
            .collect::<Option<Vec<_>>>()
    } else {
        None
    };
    let Some(row_counts) = row_counts else {
        return catalog_refuse(
            req,
            CatalogFailure::Refused(Refusal {
                status: 500,
                code: code::INTERNAL,
                message: "the catalog row-count query returned an invalid shape".to_string(),
            }),
        );
    };

    let column_rows = match catalog_rows(
        exec, &peer,
        "SELECT schema_name, table_name, column_name, data_type, is_nullable, column_default, \
                is_generated, generation_expression \
         FROM duckdb_columns() \
         WHERE database_name = current_database() AND NOT internal \
         ORDER BY schema_name, table_name, column_index",
    ) {
        Ok(rows) => rows,
        Err(failure) => return catalog_refuse(req, failure),
    };
    let constraint_rows = match catalog_rows(
        exec, &peer,
        "SELECT schema_name, table_name, constraint_type, constraint_column_names, \
                referenced_table, referenced_column_names \
         FROM duckdb_constraints() \
         WHERE database_name = current_database() \
           AND constraint_type IN ('PRIMARY KEY', 'UNIQUE', 'FOREIGN KEY') \
         ORDER BY schema_name, table_name, constraint_index",
    ) {
        Ok(rows) => rows,
        Err(failure) => return catalog_refuse(req, failure),
    };
    // duckdb_indexes() lists only the indexes CREATE INDEX made; the internal
    // ART indexes that implement PRIMARY KEY and UNIQUE column constraints are
    // not in it, which is exactly the distinction the contract wants — that
    // constraint-borne uniqueness travels in uniqueConstraints above, not here.
    let index_rows = match catalog_rows(
        exec, &peer,
        "SELECT schema_name, table_name, index_name, is_unique, expressions \
         FROM duckdb_indexes() \
         WHERE database_name = current_database() \
         ORDER BY schema_name, table_name, index_name",
    ) {
        Ok(rows) => rows,
        Err(failure) => return catalog_refuse(req, failure),
    };
    let sequence_rows = match catalog_rows(
        exec, &peer,
        "SELECT sequence_name, start_value FROM duckdb_sequences() \
         WHERE database_name = current_database() AND NOT temporary \
         ORDER BY sequence_name",
    ) {
        Ok(rows) => rows,
        Err(failure) => return catalog_refuse(req, failure),
    };

    // Assembled in the order the queries delivered — every ORDER BY above is
    // load-bearing — and looked up by (schema, name), never iterated from the
    // map, so nothing about the output depends on hash order.
    let mut tables: Vec<Table> = Vec::new();
    let mut index_of: HashMap<(String, String), usize> = HashMap::new();
    for (row, row_count) in table_rows.iter().zip(row_counts) {
        let schema = cell_str(row, 0);
        let name = cell_str(row, 1);
        index_of.insert((schema.clone(), name.clone()), tables.len());
        tables.push(Table { schema, name, row_count: Some(row_count), ddl: cell_opt_str(row, 2), ..Table::default() });
    }
    for row in &column_rows {
        let Some(&t) = index_of.get(&(cell_str(row, 0), cell_str(row, 1))) else { continue };
        tables[t].columns.push(Column {
            name: cell_str(row, 2),
            duck_type: cell_str(row, 3),
            not_null: !cell_bool(row, 4),
            default: cell_opt_str(row, 5),
            generated: cell_bool(row, 6),
            generation_expression: cell_opt_str(row, 7),
            primary: false,
        });
    }
    for row in &constraint_rows {
        let Some(&t) = index_of.get(&(cell_str(row, 0), cell_str(row, 1))) else { continue };
        let columns = cell_list(row, 3);
        match cell_str(row, 2).as_str() {
            "PRIMARY KEY" => {
                for column in tables[t].columns.iter_mut() {
                    if columns.contains(&column.name) {
                        column.primary = true;
                    }
                }
                tables[t].primary_key = columns;
            }
            "UNIQUE" => {
                tables[t].unique_constraints.push(Unique { columns });
            }
            "FOREIGN KEY" => {
                let ref_schema = tables[t].schema.clone();
                tables[t].foreign_keys.push(ForeignKey {
                    columns,
                    ref_table: cell_str(row, 4),
                    ref_schema,
                    ref_columns: cell_list(row, 5),
                });
            }
            _ => {}
        }
    }
    for row in &index_rows {
        let Some(&t) = index_of.get(&(cell_str(row, 0), cell_str(row, 1))) else { continue };
        let (mut columns, mut expressions) = (Vec::new(), Vec::new());
        for part in index_parts(&cell_str(row, 4)) {
            match part {
                IndexPart::Column(name) => columns.push(name),
                IndexPart::Expression(text) => expressions.push(text),
            }
        }
        tables[t].indexes.push(Index {
            name: cell_str(row, 2),
            columns,
            expressions,
            unique: cell_bool(row, 3),
        });
    }
    for table in tables.iter_mut() {
        // A unique constraint has no name in this shape either, so the same
        // rule: pin its position to its column list, never to storage order.
        table.unique_constraints.sort_by(|a, b| a.columns.cmp(&b.columns));
        // A foreign key has no name in this shape, so its position cannot be
        // inherited from catalog storage order; pin it to what the entry says.
        table.foreign_keys.sort_by(|a, b| {
            (&a.ref_table, &a.columns, &a.ref_columns).cmp(&(&b.ref_table, &b.columns, &b.ref_columns))
        });
    }

    let sequences = sequence_rows
        .iter()
        // The executor already applied harbor's integer policy — bare within
        // JSON's exact range, quoted past it — so the value goes out as is.
        .map(|row| Sequence { name: cell_str(row, 0), start: row.get(1).cloned().unwrap_or_default() })
        .collect();
    let catalog = Catalog { harbor_version, duckdb_version, database_size_bytes, wal_size_bytes, tables, sequences };
    let _ = req.respond(json_response(200, &serde_json::to_string(&catalog).unwrap()));
    (true, 200)
}

/// `GET /ready` — does the database actually answer?
///
/// This is deliberately not a liveness check. A static 200 says only that the
/// HTTP thread is running, which is the one thing least likely to be wrong: the
/// executor thread can be gone, the connection can be wedged, and a process that
/// answers a hardcoded string is happy to say so while every `/sql` returns 500.
/// So this runs `SELECT 1` down the same path a query takes, and reports what
/// came back.
fn run_ready(req: Request, jobs: &mpsc::SyncSender<Job>) -> (bool, u16) {
    if let Some((at, ok)) = *LAST_READY.lock().unwrap()
        && at.elapsed() < READY_MAX_AGE
    {
        return (true, respond_ready(req, ok, "not ready"));
    }

    // No deadline. A readiness probe that can time out would report the
    // database unready because harbor cancelled the probe, which is a
    // self-inflicted outage rather than a measurement.
    let (job, ready_rx, body_rx) = Job::new("SELECT 1".to_string(), Vec::new(), Shape::Ndjson, None);

    // The executor being gone is the failure this endpoint exists to catch, and
    // the one condition the worker must act on rather than merely report: it
    // returns `false` so the accept loop is left, exactly as `run_sql` does.
    if jobs.send(job).is_err() {
        *LAST_READY.lock().unwrap() = Some((Instant::now(), false));
        let _ = req.respond(error_response(503, code::UNREADY, "harbor is shutting down"));
        return (false, 503);
    }

    let verdict = ready_rx.recv();
    // Drain rather than drop. Dropping the receiver makes the executor's send
    // fail, which it reads as a client that hung up mid-stream and answers by
    // rolling back the connection before the next job — a real cost, paid once
    // a second, for a result that is four rows of nothing.
    while body_rx.recv().is_ok() {}

    match verdict {
        Ok(Ok(())) => {
            *LAST_READY.lock().unwrap() = Some((Instant::now(), true));
            (true, respond_ready(req, true, ""))
        }
        Ok(Err(refusal)) => {
            // Whatever the refusal's own status would be, a database that
            // cannot answer SELECT 1 is unready — that is the question asked.
            *LAST_READY.lock().unwrap() = Some((Instant::now(), false));
            let _ = req.respond(error_response(503, code::UNREADY, &refusal.message));
            (true, 503)
        }
        Err(_) => {
            *LAST_READY.lock().unwrap() = Some((Instant::now(), false));
            let _ = req.respond(error_response(503, code::UNREADY, "the executor thread is gone"));
            (false, 503)
        }
    }
}

/// Success is a plain status object; failure rides the same error envelope as
/// every other refusal, so one client-side reader handles both.
fn respond_ready(req: Request, ok: bool, message: &str) -> u16 {
    if ok {
        let _ = req.respond(json_response(200, r#"{"status":"ready"}"#));
        200
    } else {
        let _ = req.respond(error_response(503, code::UNREADY, message));
        503
    }
}

/// A worker's executor: its jobs channel and its cancellation slot.
type Executor<'a> = (&'a mpsc::SyncSender<Job>, &'a Arc<SlotState>);

/// `POST /sql` on a worker: the body read and parsed, then the statement run.
fn run_sql_request(mut req: Request, exec: Executor) -> (bool, u16) {
    let parsed = read_request_body(&mut req)
        .and_then(|body| parse_request(&body).map_err(Refusal::bad_request));
    match parsed {
        Ok(parsed) => run_sql(req, parsed, Some(exec)),
        Err(refusal) => refuse(req, refusal),
    }
}

/// Returns (keep serving, status sent). The first is false when the worker's
/// own executor is gone; see `handle`, which also writes the log line from
/// the second. `pooled` is the accepting worker's executor; a relay has
/// none, and brings only a session's statement.
fn run_sql(req: Request, mut parsed: SqlRequest, pooled: Option<Executor>) -> (bool, u16) {
    // `r.{a,b}` becomes `r.a, r.b` here, once, for every client: before the
    // guards below and the engine's statement count, which read the
    // statement the engine will run.
    match unbrace::expand(&parsed.sql) {
        Ok(std::borrow::Cow::Owned(expanded)) => parsed.sql = expanded,
        Ok(std::borrow::Cow::Borrowed(_)) => {}
        Err(e) => return refuse(req, Refusal::bad_request(e)),
    }

    // `USE` sets the CURRENT DATABASE on the connection it runs on, and
    // outside a session that connection is a pooled one that goes back to the
    // pool when this request ends. Since a request carries exactly one
    // statement (the engine's count refuses more, in `run_statement`), nothing
    // can ever follow it on that connection — so the USE reports success and is
    // discarded, every time. There is no case where running it is useful,
    // which is what makes refusing safe rather than merely stricter.
    //
    // A session is the connection that persists, and inside one USE works
    // normally; qualifying names (`db.schema.table`) needs no session at all.
    // Other connection-local state — temp tables, PREPARE, session-scoped
    // SET — is silently lost the same way; USE is fenced because it is the
    // one whose whole purpose is to change what the NEXT statement sees.
    //
    // `BEGIN` is fenced for the same reason, with more at stake: it answers
    // success, the transaction is gone when the request ends, and every
    // statement the client believes is inside it commits on its own. The
    // `ROLLBACK` that was meant to undo them finds nothing to undo.
    if parsed.session.is_none()
        && let Some(lost) = lost_without_session(&parsed.sql)
    {
        return refuse(req, Refusal::sql(lost));
    }

    let shape = if wants_one_shot(&req) { Shape::Json } else { Shape::Ndjson };

    // A statement naming a lease goes to that lease's connection, wherever it
    // is; everything else runs on the connection belonging to the worker that
    // accepted the request. The claim is held until this function returns —
    // `Claim` releases it on drop, so no early return can leave a lease stuck
    // busy, which would wedge it until the reaper noticed.
    let claim = match parsed.session.as_deref() {
        None => None,
        Some(id) => match lease_claim(id) {
            Ok((target, state)) => {
                Some(Claim { id: id.to_string(), sql: parsed.sql.clone(), target, state, ran: Default::default() })
            }
            Err(refusal) => return refuse(req, refusal),
        },
    };
    // Cancellation has to name the connection that will be executing, not
    // the one that accepted the request.
    let (target, slot) = match (&claim, pooled) {
        (Some(c), _) => (&c.target, &c.state),
        (None, Some(pooled)) => pooled,
        (None, None) => return shed(req),
    };

    let (job, ready, body) = Job::new(parsed.sql, parsed.params, shape, parsed.timeout);
    let id = job.id;

    // Registered before the job is sent, so a Stop pressed the instant the
    // query goes out has something to find. `Cancellable` deregisters on drop,
    // on every path below including the early returns.
    let _cancellable = match parsed.query.as_deref() {
        None => None,
        Some(name) => match Cancellable::register(name, slot, id) {
            Ok(guard) => Some(guard),
            Err(refusal) => return refuse(req, refusal),
        },
    };

    if target.send(job).is_err() {
        // A lease whose executor is gone can never serve another statement, so
        // it is not merely a failed request — the lease itself is finished.
        // The worker keeps serving; only the lease dies.
        if let Some(c) = claim {
            let id = c.id.clone();
            drop(c);
            lease_release(&id);
            return refuse(req, Refusal { status: 503, code: code::UNAVAILABLE, message: "this session is gone".into() });
        }
        return (false, refuse(req, Refusal::not_serving()).1);
    }
    // A worker that waits on a session's statement is as busy as one running
    // its own, and the probe lane, which answers the session's renewals,
    // takes over from the same age.
    if let (Some(_), Some((_, worker))) = (&claim, pooled) {
        worker.run.lock().unwrap().wedged_at = Some(Instant::now() + WEDGED_STATEMENT_AGE);
    }

    let mut watch = Watch::new(Some(req.peer()), Arc::clone(slot), id);
    let answer = watch.recv(&ready);
    if let (Some(c), Ok(outcome)) = (&claim, &answer) {
        c.ran.set(match outcome {
            Ok(()) => true,
            Err(refusal) => refusal.code == code::SQL_ERROR && !refusal.message.starts_with("Parser Error"),
        });
    }
    let refusal = match answer {
        Ok(Ok(())) => None,
        Ok(Err(refusal)) => Some(refusal),
        Err(_) => {
            let gone = Refusal { status: 500, code: code::INTERNAL, message: "the executor thread is gone".into() };
            // Only the worker's own executor gone ends the worker.
            return (claim.is_some(), refuse(req, gone).1);
        }
    };
    if let Some(refusal) = refusal {
        return refuse(req, refusal);
    }
    let mut headers = vec![
        Header::from_bytes(&b"Content-Type"[..], match shape {
            Shape::Json => wire::CONTENT_JSON,
            Shape::Ndjson => wire::CONTENT_NDJSON,
        }.as_bytes()).unwrap(),
        Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]).unwrap(),
    ];
    if shape == Shape::Json {
        // One message, because that is what the executor sends in this
        // shape — but drain the channel rather than assume it, so a
        // future change to how the document is chunked cannot silently
        // truncate a response.
        let mut document = Vec::new();
        while let Ok(chunk) = watch.recv(&body) {
            document.extend_from_slice(&chunk);
        }
        let length = document.len();
        let _ = req.respond(Response::new(200.into(), headers, std::io::Cursor::new(document), Some(length)));
        return (true, 200);
    }
    // data_length: None makes justhttp chunk the body and keep the
    // connection alive. The response is written by reading the body channel
    // to its end, and the watch goes with the reader: a client that leaves,
    // or a write that fails, stops the statement (see `Watch`).
    if wants_zstd(&req) {
        headers.push(Header::from_bytes(&b"Content-Encoding"[..], &b"zstd"[..]).unwrap());
        return match ZstdReader::new(body, watch) {
            Ok(reader) => {
                let _ = req.respond(Response::new(200.into(), headers, reader, None));
                (true, 200)
            }
            // Encoder setup fails only short of memory, and the statement
            // has been stopped with the watch.
            Err(e) => refuse(req, Refusal { status: 500, code: code::INTERNAL, message: format!("could not start encoder: {e}") }),
        };
    }
    let _ = req.respond(Response::new(200.into(), headers, ChannelReader::new(body, watch), None));
    (true, 200)
}

/// A block size as bytes: `65536`, or a `k`/`kb`/`kib` suffix on the number
/// people actually say — `64k`. DuckDB takes only a power of two from 16 KiB
/// to 256 KiB, and refusing the rest HERE rather than at open means the
/// complaint can name the flag and list the answers.
pub fn parse_block_size(s: &str) -> Result<u64, String> {
    let t = s.trim().to_ascii_lowercase();
    let digits = t.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let unit = &t[digits.len()..];
    let n: u64 = digits.parse().map_err(|_| format!("bad --block-size {s:?}"))?;
    let bytes = match unit {
        "" | "b" => n,
        "k" | "kb" | "kib" => n * 1024,
        _ => return Err(format!("bad --block-size {s:?} — use bytes or a k suffix, e.g. 64k")),
    };
    if !(16384..=262144).contains(&bytes) || !bytes.is_power_of_two() {
        return Err(format!(
            "bad --block-size {s:?} — DuckDB takes a power of two from 16k to 256k \
             (16k, 32k, 64k, 128k, 256k)"
        ));
    }
    Ok(bytes)
}

/// Settings a client must not change: they are process-global in DuckDB, so
/// one `SET memory_limit='100GB'` raises it for every neighbor berth on the
/// host and defeats the fleet-safe cap the operator chose at berth start.
/// Verified live: the SET took effect for all workers at once.
const FENCED: &[&str] = &[
    "memory_limit",
    "max_memory",
    "threads",
    "worker_threads",
    "external_threads",
    // Disk spill is process-global too, and the operator caps it with
    // `--max-temp-size` precisely so one query cannot fill the shared host
    // disk. Left unfenced, `SET max_temp_directory_size='100TB'`
    // over the wire erases that cap; `temp_directory` redirects the spill
    // itself. Both are GLOBAL-scope in DuckDB — same class as the rest here.
    "max_temp_directory_size",
    "temp_directory",
    "allowed_configs",
    "lock_configuration",
];

/// Lock [`FENCED`] in the engine, which then refuses every form that reaches
/// one (`SET`, `RESET`, `PRAGMA`, a quoted name, an analyzed `EXPLAIN`)
/// with its own message, naming the setting. Initialization runs before this
/// function.
fn lock_operator_settings(conn: &mut Connection) -> Result<(), String> {
    let locked = conn.query_strings("SELECT current_setting('lock_configuration')")
        .map_err(|e| e.into_text())?;
    if locked.first().map(String::as_str) == Some("true") {
        // An operator may lock configuration during init, but must not leave
        // any of Harbor's protected settings changeable.
        let allowed = conn.query_strings("SELECT unnest(current_setting('allowed_configs'))")
            .map_err(|e| e.into_text())?;
        if allowed.iter().any(|s| FENCED.iter().any(|f| s.eq_ignore_ascii_case(f))) {
            return Err("init locked configuration with protected settings in allowed_configs".into());
        }
        return Ok(());
    }
    let allowed = conn.query_strings("SELECT name FROM duckdb_settings() ORDER BY name")
        .map_err(|e| e.into_text())?;
    let allowed = allowed.iter()
        .filter(|s| !FENCED.contains(&s.as_str()))
        .map(|s| format!("'{}'", s.replace('\'', "''")))
        .collect::<Vec<_>>().join(",");
    conn.execute_batch(&format!("SET allowed_configs=[{allowed}]; SET lock_configuration=true"))
        .map_err(|e| format!("cannot protect operator settings: {e}"))
}

/// Refuse, outside a session, the statements whose whole purpose is to change
/// what the next statement sees: `USE` and the opening of a transaction. A
/// pooled connection is replaced before reuse, so neither can reach a later
/// one-shot request, and each would answer success for work that is already
/// lost. The answer is what the client is told.
///
/// Reads through comments, every space the engine skips and an analyzed
/// `EXPLAIN` via `acting_keyword`, so `/*x*/ USE d` and `EXPLAIN ANALYZE
/// BEGIN` are caught with the bare forms, and a table that happens to be
/// named `"begin"` is not.
fn lost_without_session(sql: &str) -> Option<&'static str> {
    match acting_keyword(sql).as_str() {
        "USE" => Some(
            "USE has no effect outside a session: this connection returns to the pool when \
             the request ends, and one request carries one statement, so nothing runs on it \
             afterward. Open a session (POST /sql/sessions) and send USE on that, or qualify \
             names instead — database.schema.table",
        ),
        "BEGIN" | "START" => Some(
            "a transaction cannot begin outside a session: this connection returns to the \
             pool when the request ends, so the transaction would end with it and every \
             statement after it would commit on its own. Open a session (POST /sql/sessions), \
             send BEGIN and the statements that follow with its sessionId, and release it \
             when the transaction is over",
        ),
        _ => None,
    }
}

/// Statements that can leave connection-local state need a fresh connection
/// before another caller uses this worker. Only known state-neutral statement
/// forms keep their parse cache; wrappers, SET, temporary DDL and unknown forms
/// take the conservative path. Pinned sessions reset only on release.
fn needs_connection_reset(sql: &str) -> bool {
    !matches!(
        bare_word(sql.as_bytes(), &mut 0).as_str(),
        "SELECT" | "WITH" | "FROM" | "VALUES" | "TABLE"
            | "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "TRUNCATE"
            | "COPY" | "EXPORT" | "CHECKPOINT" | "ANALYZE" | "VACUUM"
            | "DESCRIBE" | "SHOW" | "SUMMARIZE" | "PIVOT" | "UNPIVOT"
    )
}

/// Final rollback during server shutdown, before returning connections.
fn reset_transaction(conn: &mut Connection) {
    let _ = conn.execute_batch("ROLLBACK");
}

/// A statement registered on its slot for as long as it runs.
///
/// Dropping it retires the statement, so no path out of the loop — and there
/// are seven — can leave the slot claiming to be running a job that is over.
/// A cancel that arrives after that matches nothing, which is the point.
struct OnSlot<'a> {
    slot: &'a SlotState,
    done: bool,
}

impl OnSlot<'_> {
    /// Retire now, and say whether this statement was cancelled. Called
    /// explicitly wherever the answer changes what the client is told.
    fn finish(&mut self) -> bool {
        if self.done {
            return false;
        }
        self.done = true;
        self.slot.end()
    }

    /// Retire a statement that did not run, and say why: cancelled when a
    /// canceller reached it, which leaves its transaction aborted — a 499
    /// inside a transaction means the transaction is over, whenever the
    /// cancel arrived — else the engine's `message`. Retired first, so no
    /// interrupt is aimed at the statement that does the aborting.
    fn refuse(&mut self, conn: &mut Connection, message: String) -> Refusal {
        match self.finish() {
            true => {
                conn.abort_transaction();
                Refusal::cancelled()
            }
            false => Refusal::sql(message),
        }
    }
}

impl Drop for OnSlot<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.slot.end();
        }
    }
}

/// One statement, start to finish, on the executor's connection: prepare,
/// stream (NDJSON) or buffer (one-shot JSON) the result, and report the
/// outcome on `ready`/`body`. Returns whether the connection needs a
/// fresh engine connection before the next job. Split out of `execute_jobs` so the whole
/// thing runs under one `catch_unwind` there: a panic in the DuckDB client (a
/// decoder that hits `unreachable!`, a metadata assert) must not take the
/// executor thread — and with it a worker and a pool slot — down for good.
// Eight, and deliberately. This exists to be the whole of what runs under
// one catch_unwind in execute_jobs, so every value that unwind must not
// straddle is passed in rather than captured. Bundling them into a struct
// would hide exactly the thing the split was made to show.
#[allow(clippy::too_many_arguments)]
fn run_statement(
    conn: &mut Connection,
    on_slot: &mut OnSlot,
    sql: String,
    params: Vec<Param>,
    shape: Shape,
    ready: mpsc::SyncSender<Result<(), Refusal>>,
    body: mpsc::SyncSender<Vec<u8>>,
    started: Instant,
) -> bool {
    // Decided from the statement text before it runs, then widened below by
    // any path that ends the job early.
    let mut needs_reset = needs_connection_reset(&sql);

    // Parsed once, cached by SQL text (per-connection LRU) — a repeated
    // statement skips DuckDB's parse, the dominant engine cost for small
    // SQL and the mitigation for v2's slower parser. A cached statement is
    // raw parser output, so execution re-binds and a catalog change is
    // always seen; test/sql gates that empirically (drop/recreate a
    // referenced table, then re-run the identical text).
    let stmts = match conn.statements(&sql) {
        Ok(s) => s,
        Err(e) => {
            let _ = ready.send(Err(on_slot.refuse(conn, e.into_text())));
            return true;
        }
    };
    let Some((last, front)) = stmts.split_last() else {
        // Whitespace and comments parse to no statements at all.
        let _ = ready.send(Err(on_slot.refuse(conn, "No statements to prepare from".to_string())));
        return true;
    };

    // One statement per request, counted by the engine's own parser on the
    // text it would run, before anything runs: parsing is side-effect free,
    // and no reading of the text but the engine's can say what it holds.
    if !front.is_empty() {
        on_slot.finish();
        let _ = ready.send(Err(Refusal {
            status: 400,
            code: code::BAD_REQUEST,
            message: "exactly one SQL statement is allowed per request".into(),
        }));
        return needs_reset;
    }
    // A document aimed at a VARIANT is bound as one. Finding that out is a
    // bind pass and making it one is a cast, and an interrupt that lands
    // during either is dropped by the engine. So the values are built first
    // and the slot is asked again once they are, before anything runs.
    let mut params = params;
    let bound = match conn.bind(last, &mut params) {
        Ok(b) => b,
        Err(e) => {
            let _ = ready.send(Err(on_slot.refuse(conn, e.into_text())));
            return true;
        }
    };
    if on_slot.slot.cancelled() {
        let _ = ready.send(Err(on_slot.refuse(conn, String::new())));
        return needs_reset;
    }
    // A COMMIT runs to its answer. An interrupt that reaches one as it
    // finishes is reported by the engine on the fetch that follows, after
    // the transaction is durable, and the client would be told 499 for work
    // that was kept; told that, it may do the work again. So the slot is
    // retired before a COMMIT starts and nothing is aimed at it: a cancel
    // either arrived by now, and is answered as one with nothing kept, or
    // finds no statement to stop. The answer is then the engine's own.
    if commits(&sql) && on_slot.finish() {
        conn.abort_transaction();
        let _ = ready.send(Err(Refusal::cancelled()));
        return needs_reset;
    }
    let mut stream = match conn.execute(last, bound) {
        Ok(s) => s,
        Err(e) => {
            let _ = ready.send(Err(on_slot.refuse(conn, e.into_text())));
            return true;
        }
    };
    let columns = std::mem::take(&mut stream.columns);
    let api = conn.api();
    // The VARIANT caster, one per connection. Every engine harbor ships has
    // the cast route; one that lacks it can still answer any statement whose
    // result holds no VARIANT, and refuses the rest rather than send display
    // text under a schema line that promised JSON.
    let json = match conn.json() {
        Ok(j) => Some(j),
        Err(e) => {
            if columns.iter().any(|(_, ty)| crate::engine::encode::holds_variant(ty)) {
                drop(stream);
                let _ = ready.send(Err(on_slot.refuse(conn, e.into_text())));
                return needs_reset;
            }
            None
        }
    };

    // NDJSON commits to a 200 here, before the first row, because that is
    // what streaming means. One-shot cannot and must not: nothing goes out
    // until the result is whole, so a failure at row 900,000 is still free
    // to be a 400 rather than a 200 with an apology inside it. The
    // handshake therefore moves to the bottom of the loop in that shape.
    if shape == Shape::Ndjson && ready.send(Ok(())).is_err() {
        return true;
    }

    // Small results (the common case) use a few hundred bytes; start small
    // and let a large result grow toward FLUSH_AT instead of paying a 72KB
    // large-path allocation per statement. After the first flush, each
    // refill below allocates one full-capacity buffer per 64KB flushed —
    // one clean malloc per flush beats the grow path's cascade of reallocs.
    let mut buf = String::with_capacity(4096);
    match shape {
        Shape::Ndjson => buf.push_str(r#"{"type":"schema","columns":["#),
        // No `kind` ("select" or "write") is emitted, because there is no
        // definition of it that is right: DuckDB answers CREATE TABLE with a one-column `Count`
        // result, so "did the statement produce columns" calls a write a
        // select, and deciding from the leading keyword is a parser that
        // exists only to label something no client needs — `columns` and
        // `rowCount` already say everything it could. A field that is
        // absent is easier to handle than one that lies.
        Shape::Json => buf.push_str(r#"{"ok":true,"columns":["#),
    }
    for (i, (name, ty)) in columns.iter().enumerate() {
        if i > 0 {
            buf.push(',');
        }
        crate::engine::encode::emit_column_schema(&mut buf, Some(name), ty);
    }
    match shape {
        Shape::Ndjson => buf.push_str("]}\n"),
        Shape::Json => buf.push_str(r#"],"data":["#),
    }

    let mut count: u64 = 0;
    let mut gone = false;
    // The statement stopped short of its end by a cancel or by its reader
    // leaving, which in a transaction is a statement missing from it.
    let mut cut_short = false;
    // Set when the result cannot be completed. In NDJSON it has already
    // been written into the stream by the time it is set; in one-shot it is
    // what the request fails with.
    let mut failure: Option<Refusal> = None;
    'stream: loop {
        // A fetch failure is the SQL-failed / cancelled path: v2 reports a
        // cancellation as ERROR_RUNTIME_INTERRUPT on the fetch, and only the
        // slot knows which of the two this was.
        let fetched = stream.next_chunk().and_then(|next| match next {
            None => Ok(None),
            Some(chunk) => chunk.readers(columns.len()).map(|r| Some((chunk, r))),
        });
        let (chunk, readers) = match fetched {
            Ok(Some(pair)) => pair,
            Ok(None) => break,
            Err(e) => {
                // Retired here rather than after the loop, because what the
                // client is told depends on the answer: the same DuckDB
                // error means "your SQL failed" or "you cancelled this",
                // and only the slot knows which.
                let cancelled = on_slot.finish();
                cut_short |= cancelled;
                let refusal = refusal_for(cancelled, e.into_text());
                if shape == Shape::Ndjson {
                    // Mid-stream failures cannot change the status code —
                    // the headers are long gone. Say so in the stream, so a
                    // client never mistakes a truncated result for a
                    // complete one.
                    push_error(&mut buf, refusal.code, &refusal.message);
                    buf.push('\n');
                    let _ = body.send(std::mem::take(&mut buf).into_bytes());
                    gone = true;
                }
                failure = Some(refusal);
                break;
            }
        };

        for row in 0..chunk.rows {
            // Encoded straight into `buf` behind a mark: a failing cell
            // discards the half-written row with truncate(). Cells are read
            // from vector views, and an engine failure mid-cell surfaces as
            // an Err and fails the stream honestly.
            let mark = buf.len();
            match shape {
                Shape::Ndjson => buf.push_str(r#"{"type":"row","values":["#),
                Shape::Json => {
                    if count > 0 {
                        buf.push(',');
                    }
                    buf.push('[');
                }
            }
            let mut cell_err = None;
            for (i, ((_, ty), reader)) in columns.iter().zip(&readers).enumerate() {
                if i > 0 {
                    buf.push(',');
                }
                if let Err(e) = crate::engine::encode::emit_cell(&mut buf, api, json, reader, ty, row) {
                    cell_err = Some(e);
                    break;
                }
            }
            if let Some(e) = cell_err {
                buf.truncate(mark);
                // The statement's own error, when one waits behind the cell's.
                let e = stream.error_after(e);
                let cancelled = on_slot.finish();
                cut_short |= cancelled;
                let refusal = refusal_for(cancelled, e.into_text());
                if shape == Shape::Ndjson {
                    push_error(&mut buf, refusal.code, &refusal.message);
                    buf.push('\n');
                    let _ = body.send(std::mem::take(&mut buf).into_bytes());
                    gone = true;
                }
                failure = Some(refusal);
                break 'stream;
            }
            match shape {
                Shape::Ndjson => buf.push_str("]}\n"),
                Shape::Json => buf.push(']'),
            }

            count += 1;

            match shape {
                Shape::Ndjson => {
                    if buf.len() >= FLUSH_AT {
                        // A send failure means the client hung up.
                        // Abandon the query rather than finish
                        // computing a result nobody will read.
                        if body.send(std::mem::take(&mut buf).into_bytes()).is_err() {
                            gone = true;
                            cut_short = true;
                            break 'stream;
                        }
                        buf = String::with_capacity(FLUSH_AT + 8192);
                    }
                }
                // Nothing can be flushed in this shape — the document
                // is not valid until its last byte — so the only
                // protection against a result larger than memory is to
                // refuse. Streaming has no such limit, and is the
                // default, so the remedy is always available.
                Shape::Json => {
                    if buf.len() > MAX_JSON_RESPONSE {
                        // 406, not 413: nothing is wrong with the
                        // request or its size. What cannot be done is
                        // producing this result in the representation
                        // the Accept header asked for — which is
                        // exactly what "not acceptable" means, and the
                        // message names the one that would work.
                        failure = Some(Refusal {
                            status: 406,
                            code: code::RESPONSE_TOO_LARGE,
                            message: format!(
                                "this result is larger than the {} MiB harbor will hold \
                                 in memory for a single JSON document. Ask for NDJSON \
                                 instead — send no Accept header, or Accept: \
                                 application/x-ndjson — and it streams with no size \
                                 limit.",
                                MAX_JSON_RESPONSE >> 20
                            ),
                        });
                        break 'stream;
                    }
                }
            }
        }
    }
    // The cursor releases the connection for whatever runs next; the tail
    // below only writes bytes.
    drop(stream);
    // A statement cut short inside a transaction is missing from it. The
    // interrupt that ends the cursor aborts the transaction only when it
    // lands inside the engine, which is a matter of timing; it is left
    // aborted every time, so the COMMIT that follows is told. In autocommit
    // this leaves nothing behind. Retired first, so no interrupt is aimed at
    // the statement that does the aborting.
    if cut_short {
        on_slot.finish();
        conn.abort_transaction();
    }

    // An abandoned or failed stream is the case that poisons a connection.
    needs_reset = needs_reset || gone || failure.is_some();

    match shape {
        Shape::Ndjson => {
            if !gone {
                let _ = write!(
                    buf,
                    r#"{{"type":"end","rowCount":{},"timeMs":{}}}"#,
                    count,
                    started.elapsed().as_millis()
                );
                buf.push('\n');
                let _ = body.send(buf.into_bytes());
            }
        }
        // The deferred handshake, and the whole reason this shape waits:
        // a failure at the last row is still a status code and a code the
        // client can classify on, rather than a 200 with an apology in the
        // body. Both travel on the refusal, so the same failure reports the
        // same code in either shape.
        Shape::Json => match failure {
            Some(message) => {
                let _ = ready.send(Err(message));
            }
            None => {
                let _ = write!(
                    buf,
                    r#"],"rowCount":{},"timeMs":{}}}"#,
                    count,
                    started.elapsed().as_millis()
                );
                if ready.send(Ok(())).is_err() {
                    return true;
                }
                let _ = body.send(buf.into_bytes());
            }
        },
    }
    needs_reset
}

/// The DuckDB side. Owns a connection slot for the life of the server and runs
/// one statement at a time; concurrency comes from there being several of
/// these, not from any one of them interleaving work. `pinned` marks a lease
/// connection: the per-job reset that stops one request's stray transaction
/// from leaking into the next request on the same connection must not fire on
/// a lease, because holding that transaction open is precisely what a lease is
/// for. Connection replacement happens on DELETE or expiry; COMMIT alone
/// keeps the session and its local state.
fn execute_jobs(
    mut conn: Connection,
    jobs: mpsc::Receiver<Job>,
    pinned: bool,
    state: Arc<SlotState>,
) -> Connection {
    let mut needs_reset = false;
    // A failed replacement must never make the contaminated handle usable,
    // including when a released lease is assigned to another caller.
    let mut must_reset = false;
    for job in jobs {
        let Job { sql, params, shape, id, deadline, reset, ready, body } = job;
        must_reset |= reset || (needs_reset && !pinned);
        if must_reset {
            if let Err(e) = conn.reset() {
                let _ = ready.send(Err(Refusal {
                    status: 503,
                    code: code::UNAVAILABLE,
                    message: format!("cannot reset connection: {e}"),
                }));
                continue;
            }
            needs_reset = false;
            must_reset = false;
        }
        if reset {
            let _ = ready.send(Ok(()));
            continue;
        }
        let started = Instant::now();

        // Registered before `prepare`, not before the row loop: planning a
        // pathological query can itself take minutes, and a statement that
        // cannot be cancelled until it starts producing rows is exactly the
        // statement worth cancelling.
        let pre_cancelled = state.begin(id, deadline);
        let mut on_slot = OnSlot { slot: &state, done: false };
        if pre_cancelled {
            // Cancelled between being registered and being picked up. Nothing
            // ran, and the transaction it would have run in is left aborted,
            // as a cancel that lands at any later moment leaves it.
            on_slot.finish();
            conn.abort_transaction();
            let _ = ready.send(Err(Refusal::cancelled()));
            continue;
        }

        // A COMMIT cannot keep what an aborted transaction held: the engine
        // rolls it back and answers success, and the client that reads that
        // answer believes its work was kept. So a session's COMMIT is
        // preceded by the question, and an aborted transaction is rolled
        // back here and answered as what happened. A cancel that lands on
        // the question fails it too, and is left for the statement to
        // report. The slot is retired before the ROLLBACK, so no interrupt is
        // aimed at it; a cancel that arrived in between has had its effect,
        // an aborted transaction, and is answered as a cancel.
        if pinned
            && commits(&sql)
            && conn.transaction_aborted()
            && !state.cancelled()
        {
            if on_slot.finish() {
                let _ = ready.send(Err(Refusal::cancelled()));
                continue;
            }
            let message = match conn.execute_batch("ROLLBACK") {
                Ok(_) => "the transaction was aborted by an earlier error or a cancelled \
                          statement, so there was nothing this COMMIT could keep: it has been \
                          rolled back, and nothing since BEGIN was kept"
                    .to_string(),
                Err(e) => format!(
                    "the transaction was aborted by an earlier error or a cancelled statement, \
                     so there is nothing this COMMIT can keep, and rolling it back failed ({}): \
                     send ROLLBACK, or release the session",
                    e.into_text()
                ),
            };
            let _ = ready.send(Err(Refusal { status: 400, code: code::SQL_ERROR, message }));
            continue;
        }

        // A panic below — an encoder invariant tripping, an FFI metadata
        // assert (the v2 paths return Err rather than panic, so this is the
        // backstop, not the expectation) — must not unwind out of this
        // thread: the worker would find the job channel closed, leave the
        // accept loop, and the slot would be gone for the life of the
        // process, so a handful of such queries would retire every worker.
        // On a panic the `OnSlot` guard drops — retiring the slot — the
        // waiting worker is told (500), and this executor takes the next job.
        // The connection itself is intact (the panic was in Rust-side
        // encoding, not DuckDB's engine), so the next job resets first.
        let ready_guard = ready.clone();
        needs_reset = match std::panic::catch_unwind(AssertUnwindSafe(|| {
            run_statement(&mut conn, &mut on_slot, sql, params, shape, ready, body, started)
        })) {
            Ok(next_reset) => next_reset,
            Err(_) => {
                let _ = ready_guard.send(Err(Refusal {
                    status: 500,
                    code: code::INTERNAL,
                    message: "harbor recovered from an internal error while \
                              handling this statement"
                        .to_string(),
                }));
                true
            }
        };
    }
    // And once more on the way out, so a connection going back to the pool for
    // the next start() is clean too. Unconditional here: this runs once
    // per server lifetime, so the extra statement costs nothing.
    reset_transaction(&mut conn);
    conn
}

/// How often a wait on a statement looks at its client.
const WATCH_EVERY: Duration = Duration::from_millis(100);

/// A client's statement, watched for the client: the waits for its answer
/// and its rows look at the connection between batches, and a client that
/// has hung up has its statement stopped, by id — before any row of it was
/// written, in the minutes a plan computes before its first row, as well as
/// mid-stream. Dropped before the body channel was read to its end, as when
/// a response write failed, it stops the statement too. A statement that has
/// finished is found by neither, and a COMMIT has given up its slot before it
/// runs, so it runs to its answer.
struct Watch {
    peer: Option<justhttp::Peer>,
    slot: Arc<SlotState>,
    id: u64,
    /// The body channel was read to its end: the statement is over.
    ended: bool,
}

impl Watch {
    fn new(peer: Option<justhttp::Peer>, slot: Arc<SlotState>, id: u64) -> Self {
        Self { peer, slot, id, ended: false }
    }

    /// `rx.recv()`, stopping the statement once its client is gone.
    fn recv<T>(&mut self, rx: &mpsc::Receiver<T>) -> Result<T, mpsc::RecvError> {
        loop {
            match rx.recv_timeout(WATCH_EVERY) {
                Ok(v) => return Ok(v),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.ended = true;
                    return Err(mpsc::RecvError);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if self.peer.take_if(|p| p.closed()).is_some() {
                        self.slot.cancel(Some(self.id));
                    }
                }
            }
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        if !self.ended {
            self.slot.cancel(Some(self.id));
        }
    }
}

/// Adapts the body channel to the `Read` justhttp wants. Returning `Ok(0)`
/// when the sender is dropped is what ends the chunked response.
struct ChannelReader {
    rx: mpsc::Receiver<Vec<u8>>,
    current: Vec<u8>,
    pos: usize,
    watch: Watch,
}

impl ChannelReader {
    fn new(rx: mpsc::Receiver<Vec<u8>>, watch: Watch) -> Self {
        Self { rx, current: Vec::new(), pos: 0, watch }
    }
}

impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.pos >= self.current.len() {
            match self.watch.recv(&self.rx) {
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

/// Did the client offer zstd — the one coding harbor speaks? A token scan
/// of Accept-Encoding honoring `;q=0` exclusions. Everything else is
/// identity: gzip deliberately (its encoder would throttle the stream),
/// and lz4 was measured out — within 10% of zstd's wall in its best case,
/// 2-24x less dense everywhere.
fn wants_zstd(req: &Request) -> bool {
    req.headers().iter().filter(|h| h.field.equiv("Accept-Encoding")).any(|h| {
        h.value.as_str().split(',').any(|token| {
            let mut parts = token.split(';');
            let name = parts.next().unwrap_or("").trim();
            name.eq_ignore_ascii_case("zstd")
                && !parts.any(|p| p.trim().eq_ignore_ascii_case("q=0"))
        })
    })
}

/// The body channel as one zstd frame — the `Content-Encoding: zstd` path.
///
/// Each chunk the executor sends is written into the frame and flushed, so
/// the stream's latency profile is the uncompressed one: the executor
/// already batches to FLUSH_AT (64KB), and the channel closing finishes
/// the frame. Standard frame format, so `curl --compressed` decodes it
/// natively and `curl | zstd -d` recovers the NDJSON byte-for-byte.
///
/// The encoder writes into its own inner `Vec`, which read() drains via
/// `get_mut` between writes; `finish()` hands the Vec back with the frame
/// footer appended. Single-threaded throughout — justhttp drives this Read
/// from one writer thread.
struct ZstdReader {
    rx: mpsc::Receiver<Vec<u8>>,
    /// Some while the frame is open; None once finish() moved the buffer
    /// (footer included) into `tail`.
    enc: Option<zstd::stream::Encoder<'static, Vec<u8>>>,
    tail: Vec<u8>,
    pos: usize,
    watch: Watch,
}

impl ZstdReader {
    fn new(rx: mpsc::Receiver<Vec<u8>>, watch: Watch) -> std::io::Result<Self> {
        // Level 1: the fast end. The stream is envelope-heavy NDJSON,
        // which crushes at any level; what matters is staying off the
        // encode critical path.
        let enc = zstd::stream::Encoder::new(Vec::new(), 1)?;
        Ok(Self { rx, enc: Some(enc), tail: Vec::new(), pos: 0, watch })
    }
}

impl Read for ZstdReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        use std::io::Write;
        loop {
            let held = match &self.enc {
                Some(enc) => enc.get_ref(),
                None => &self.tail,
            };
            if self.pos < held.len() {
                let n = (held.len() - self.pos).min(out.len());
                out[..n].copy_from_slice(&held[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            let Some(enc) = self.enc.as_mut() else {
                return Ok(0);
            };
            enc.get_mut().clear();
            self.pos = 0;
            match self.watch.recv(&self.rx) {
                Ok(chunk) => {
                    enc.write_all(&chunk)?;
                    enc.flush()?;
                }
                // Sender gone: end the frame. finish() returns the buffer
                // with the last block and frame footer appended.
                Err(_) => self.tail = self.enc.take().expect("checked above").finish()?,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Responses
//
// ---------------------------------------------------------------------------

fn json_response(status: u16, body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body)
        .with_status_code(status)
        .with_header(Header::from_bytes(&b"Content-Type"[..], wire::CONTENT_JSON.as_bytes()).unwrap())
}

// The last refusal code produced on this worker thread. `handle` reads it to
// name the reason in the log line on a 4xx/5xx, without every route carrying
// the code back through its `(bool, u16)` return. Cleared at the top of each
// request and read only on a failure, so a success never reports a stale
// code. Same thread throughout: `error_response` runs inside `handle`'s
// synchronous flow, and the streamed body — written by the executor thread —
// never goes through here.
thread_local! {
    static LAST_REASON: std::cell::Cell<&'static str> = const { std::cell::Cell::new("") };
}

fn error_response(status: u16, code: &'static str, message: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    LAST_REASON.with(|c| c.set(code));
    let mut s = String::new();
    push_error(&mut s, code, message);
    json_response(status, &s)
}

/// The error envelope: the body of every refusal, and the last line of a
/// stream that failed.
fn push_error(out: &mut String, code: &str, message: &str) {
    out.push_str(r#"{"type":"error","code":"#);
    push_json_string(out, code);
    out.push_str(r#","message":"#);
    push_json_string(out, message);
    out.push('}');
}

/// Answer a request with a refusal: (keep serving, status sent).
fn refuse(req: Request, r: Refusal) -> (bool, u16) {
    let _ = req.respond(error_response(r.status, r.code, &r.message));
    (true, r.status)
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::Method;
    use super::lost_without_session;

    /// USE outside a session is refused, not silently discarded: one request
    /// carries one statement, so nothing can follow it on that connection.
    #[test]
    fn use_is_fenced_when_no_session_holds_the_connection() {
        for sql in ["USE mydb", "use mydb", "  USE  mydb ", "/*x*/ USE mydb", "--c\nUSE mydb"] {
            assert!(lost_without_session(sql).is_some_and(|m| m.starts_with("USE")), "should be fenced: {sql:?}");
        }
        // Statements that merely mention the word are untouched.
        for sql in ["SELECT 'USE'", "CREATE TABLE use_log(i INT)", "SELECT * FROM t"] {
            assert!(lost_without_session(sql).is_none(), "should pass: {sql:?}");
        }
    }

    #[test]
    fn a_transaction_cannot_begin_where_no_session_holds_the_connection() {
        for sql in [
            "BEGIN", "begin;", "BEGIN TRANSACTION", "START TRANSACTION", "  begin  transaction read only",
            "/* x */ BEGIN", "--c\nSTART TRANSACTION",
        ] {
            assert!(
                lost_without_session(sql).is_some_and(|m| m.starts_with("a transaction")),
                "should be fenced: {sql:?}"
            );
        }
        // Behind every space the engine skips, and behind an EXPLAIN that
        // runs what it explains.
        for sql in [
            "\u{feff}BEGIN", "\u{a0}BEGIN", "\u{b}BEGIN", "\u{2003}\u{3000}begin", "BEGIN\u{a0}TRANSACTION",
            "EXPLAIN ANALYZE BEGIN", "EXPLAIN ANALYSE BEGIN", "explain (analyze) begin",
            "EXPLAIN (FORMAT JSON, ANALYZE) BEGIN", "EXPLAIN ANALYZE (FORMAT JSON) BEGIN", "EXPLAIN (ANALYZE false) BEGIN",
        ] {
            assert!(lost_without_session(sql).is_some(), "should be fenced: {sql:?}");
        }
        // The other end of a transaction is the engine's to answer: with none
        // open it says so itself. A word that only starts the same is no
        // keyword, a quoted one is a table's name, and an EXPLAIN that only
        // plans begins nothing.
        for sql in [
            "COMMIT", "ROLLBACK", "END", "ABORT", "SELECT 'BEGIN'", "CREATE TABLE beginnings(i INT)", "FROM starts",
            "\"begin\"", "\"start\"", "\"use\"", "BEGIN$x", "begin_x", "BEGIN\u{e9}", "EXPLAIN BEGIN",
            "EXPLAIN (FORMAT JSON) BEGIN", "EXPLAIN (FORMAT JSON) ANALYZE BEGIN",
        ] {
            assert!(lost_without_session(sql).is_none(), "should pass: {sql:?}");
        }
    }

    use super::route_exists;
    use super::index_columns;
    use super::{IndexPart, catalog_count_sql, index_parts};
    use super::{Cancel, SlotRun};
    use std::time::{Duration, Instant};

    #[test]
    fn delivered_body_limit_checks_the_extra_byte() {
        let mut input = vec![b' '; super::MAX_BODY];
        assert_eq!(super::read_body(input.as_slice(), 0).ok().unwrap().len(), super::MAX_BODY);
        input.push(b'x');
        assert_eq!(super::read_body(input.as_slice(), 0).err().unwrap().status, 413);
        assert_eq!(super::read_body(&b"\xff"[..], 0).err().unwrap().status, 400);
    }

    #[test]
    fn engine_policy_blocks_wrappers_and_cannot_be_unlocked() {
        if crate::engine::engine().is_err() { return; }
        let mut conn = crate::engine::conn::open(std::path::Path::new(":memory:"), &[]).unwrap();
        super::lock_operator_settings(&mut conn).unwrap();
        for sql in [
            "EXPLAIN ANALYZE SET threads=2",
            "EXPLAIN ANALYZE SET worker_threads=2",
            "EXPLAIN ANALYZE SET max_memory='1GB'",
            "EXPLAIN ANALYZE RESET memory_limit",
            "EXPLAIN ANALYZE SET allowed_configs=['threads']",
            "EXPLAIN ANALYZE SET lock_configuration=false",
        ] {
            assert!(conn.execute_batch(sql).is_err(), "policy bypass: {sql}");
        }
        conn.execute_batch("SET default_order='DESC'").unwrap();
        // A pre-locked safe configuration remains valid on initialization.
        super::lock_operator_settings(&mut conn).unwrap();
    }

    #[test]
    fn engine_statement_count_is_checked_before_any_execution() {
        if crate::engine::engine().is_err() { return; }
        let mut conn = crate::engine::conn::open(std::path::Path::new(":memory:"), &[]).unwrap();
        let state = super::SlotState::new(conn.interrupt_handle());
        state.begin(1, None);
        let mut slot = super::OnSlot { slot: &state, done: false };
        let (ready, result) = std::sync::mpsc::sync_channel(1);
        let (body, _output) = std::sync::mpsc::sync_channel(1);
        super::run_statement(&mut conn, &mut slot,
            "CREATE TABLE must_not_exist(x INTEGER); SELECT 1".into(), vec![],
            super::Shape::Json, ready, body, Instant::now());
        assert_eq!(result.recv().unwrap().err().unwrap().status, 400);
        assert!(conn.execute_batch("SELECT * FROM must_not_exist").is_err());
    }

    /// A session's COMMIT on a transaction an error aborted is answered as
    /// the rollback it is, and one on a healthy transaction commits. A
    /// statement abandoned by its reader leaves the transaction aborted.
    #[test]
    fn a_commit_is_told_when_its_transaction_was_aborted() {
        use std::sync::{Arc, mpsc::sync_channel};
        if crate::engine::engine().is_err() { return; }
        let conn = crate::engine::conn::open(std::path::Path::new(":memory:"), &[]).unwrap();
        let state = super::SlotState::new(conn.interrupt_handle());
        let (jobs, queue) = sync_channel::<super::Job>(1);
        let executor = {
            let state = Arc::clone(&state);
            std::thread::spawn(move || super::execute_jobs(conn, queue, true, state))
        };
        // Send one statement; `read` says whether its rows are read or the
        // reader walks away after the first flush.
        let run = |sql: &str, read: bool| -> Result<String, (u16, String)> {
            let (ready, answered) = sync_channel(1);
            let (body, output) = sync_channel(super::BODY_QUEUE);
            jobs.send(super::Job {
                sql: sql.into(), params: vec![], shape: super::Shape::Ndjson, id: super::next_job_id(),
                deadline: None, reset: false, ready, body,
            }).unwrap();
            match answered.recv().unwrap() {
                Err(refusal) => Err((refusal.status, refusal.message)),
                Ok(()) if !read => {
                    let _ = output.recv();
                    drop(output);
                    Ok(String::new())
                }
                // A failure once the stream is open arrives in it.
                Ok(()) => {
                    let out: String = output.iter().map(|chunk| String::from_utf8(chunk).unwrap()).collect();
                    if out.contains(r#"{"type":"error""#) { Err((200, out)) } else { Ok(out) }
                }
            }
        };
        let count = |run: &dyn Fn(&str, bool) -> Result<String, (u16, String)>| {
            let out = run("SELECT count(*) FROM t", true).unwrap();
            out.lines().find(|l| l.contains("\"row\"")).unwrap().to_string()
        };
        run("CREATE TABLE t(n INTEGER PRIMARY KEY)", true).unwrap();

        // A healthy transaction commits.
        for sql in ["BEGIN", "INSERT INTO t VALUES (1)", "COMMIT"] {
            run(sql, true).unwrap();
        }
        assert!(count(&run).contains("[1]"));

        // One an error aborted does not, and the COMMIT says so.
        for sql in ["BEGIN", "INSERT INTO t VALUES (2)"] {
            run(sql, true).unwrap();
        }
        assert!(run("INSERT INTO t VALUES (1)", true).is_err(), "the duplicate key is refused");
        let (status, message) = run("COMMIT", true).unwrap_err();
        assert_eq!(status, 400);
        assert!(message.contains("rolled back") && message.contains("nothing since BEGIN"), "{message}");
        // Rolled back, with no transaction left behind it.
        assert!(count(&run).contains("[1]"));
        assert!(run("ROLLBACK", true).is_err(), "no transaction is open after it");

        // In every spelling the engine runs as a commit.
        for commit in ["end", "/* c */ COMMIT;", "EXPLAIN ANALYZE COMMIT"] {
            run("BEGIN", true).unwrap();
            assert!(run("SELECT nope", true).is_err());
            assert_eq!(run(commit, true).unwrap_err().0, 400, "{commit}");
        }

        // A statement whose reader left mid-stream is missing from its
        // transaction, which is left aborted every time.
        for sql in ["BEGIN", "INSERT INTO t VALUES (3)"] {
            run(sql, true).unwrap();
        }
        run("SELECT range, repeat('x', 200) FROM range(200000)", false).unwrap();
        let (_, message) = run("SELECT 1", true).unwrap_err();
        assert!(message.contains("aborted"), "{message}");
        assert_eq!(run("COMMIT", true).unwrap_err().0, 400);
        assert!(count(&run).contains("[1]"));

        drop(jobs);
        executor.join().unwrap();
    }

    /// A 499 inside a transaction means the transaction is over, whenever the
    /// cancel arrived: while the statement's params were being built, or
    /// before an executor had picked the statement up.
    #[test]
    fn an_early_cancel_aborts_the_transaction_it_lands_in() {
        use std::sync::mpsc::sync_channel;
        if crate::engine::engine().is_err() { return; }
        let open = || {
            let mut conn = crate::engine::conn::open(std::path::Path::new(":memory:"), &[]).unwrap();
            conn.execute_batch("CREATE TABLE m(n INTEGER); BEGIN; INSERT INTO m VALUES (1)").unwrap();
            let state = super::SlotState::new(conn.interrupt_handle());
            (conn, state)
        };
        let count = |conn: &mut super::Connection| {
            conn.query_strings("SELECT count(*)::VARCHAR FROM m").unwrap().join(",")
        };

        // The cancel fires once the slot holds the statement and before the
        // statement runs, which is where one that lands during the bind is
        // found. The engine drops the interrupt itself.
        let (mut conn, state) = open();
        let cancelled = |conn: &mut super::Connection, id: u64, sql: &str| {
            state.begin(id, None);
            assert!(state.cancel(Some(id)));
            let mut slot = super::OnSlot { slot: &state, done: false };
            let (ready, result) = sync_channel(1);
            let (body, _output) = sync_channel(1);
            super::run_statement(conn, &mut slot, sql.into(), vec![], super::Shape::Json,
                ready, body, Instant::now());
            result.recv().unwrap().err().unwrap().status
        };
        assert_eq!(cancelled(&mut conn, 1, "INSERT INTO m VALUES (2)"), 499);
        let err = conn.execute_batch("SELECT 1").unwrap_err().to_string();
        assert!(err.contains("Current transaction is aborted"), "{err}");
        conn.execute_batch("ROLLBACK").unwrap();
        assert_eq!(count(&mut conn), "0");
        // In autocommit the same cancel leaves nothing behind.
        assert_eq!(cancelled(&mut conn, 2, "INSERT INTO m VALUES (3)"), 499);
        conn.execute_batch("INSERT INTO m VALUES (4)").unwrap();
        assert_eq!(count(&mut conn), "1");

        // The cancel is held for a statement no executor has begun.
        let (conn, state) = open();
        assert!(state.cancel(Some(7)));
        let (jobs, queue) = sync_channel(4);
        let mut answers = Vec::new();
        for (id, sql) in [(7, "INSERT INTO m VALUES (2)"), (8, "SELECT 1"), (9, "ROLLBACK"),
            (10, "SELECT count(*) FROM m")] {
            let (ready, result) = sync_channel(1);
            let (body, output) = sync_channel(4);
            jobs.send(super::Job { sql: sql.into(), params: vec![], shape: super::Shape::Json, id,
                deadline: None, reset: false, ready, body }).unwrap();
            answers.push((result, output));
        }
        drop(jobs);
        drop(super::execute_jobs(conn, queue, true, state));
        let mut answers = answers.into_iter().map(|(result, output)| {
            let body = output.try_iter().flatten().collect::<Vec<u8>>();
            (result.recv().unwrap().map_err(|r| (r.status, r.message)), String::from_utf8(body).unwrap())
        });
        assert_eq!(answers.next().unwrap().0.unwrap_err().0, 499);
        let (status, message) = answers.next().unwrap().0.unwrap_err();
        assert!(status == 400 && message.contains("Current transaction is aborted"), "{status} {message}");
        assert!(answers.next().unwrap().0.is_ok());
        let (answer, body) = answers.next().unwrap();
        assert!(answer.is_ok() && body.contains(r#""data":[[0]]"#), "{body}");
    }

    #[test]
    fn a_document_param_nests_at_most_100_levels() {
        let request = |param: String| {
            super::parse_request(&format!(r#"{{"sql":"SELECT ?","params":[{param}]}}"#))
        };
        for (open, close) in [("[", "]"), (r#"{"a":"#, "}")] {
            let nested = |n: usize| format!("{}1{}", open.repeat(n), close.repeat(n));
            let fits = request(nested(100)).unwrap();
            assert!(matches!(fits.params[..], [super::Param::Document { .. }]), "{open}");
            let err = request(nested(101)).err().unwrap();
            assert_eq!(err, "a document param nests at most 100 levels", "{open}");
            // Past 127 the parser refuses first, in the same words.
            for n in [125, 127, 128, 5000] {
                assert_eq!(request(nested(n)).err().unwrap(), err, "{open} {n}");
            }
            // The deep branch need not be the first one.
            let err = request(format!(r#"[1, {{"k": {}}}, 2]"#, nested(99))).err().unwrap();
            assert_eq!(err, "a document param nests at most 100 levels", "{open}");
            assert!(request(format!(r#"[1, {{"k": {}}}, 2]"#, nested(98))).is_ok(), "{open}");
        }
        // A string is data, however deep the JSON it spells.
        let text = request(format!("\"{}{}\"", "[".repeat(150), "]".repeat(150))).unwrap();
        assert!(matches!(text.params[..], [super::Param::Text(_)]));
    }

    /// A whole number past 64 bits is refused, not bound as the nearest
    /// double; a double past them, written as one, binds as itself.
    #[test]
    fn a_number_param_binds_as_itself_or_is_refused() {
        let request = |param: &str| super::parse_request(&format!(r#"{{"sql":"SELECT ?","params":[1, {param}]}}"#));
        for whole in ["123456789012345678901234", "-9223372036854775809", "18446744073709551616"] {
            assert!(request(whole).err().unwrap().contains("send it as a string"), "{whole}");
        }
        for (text, want) in [("1.5e30", 1.5e30), ("123456789012345678901234.0", 1.2345678901234568e23),
            ("-976.7280889488817", -976.7280889488817), ("1e19", 1e19)] {
            let params = request(text).unwrap().params;
            assert!(matches!(params[1], super::Param::F64(f) if f == want), "{text}");
        }
        assert!(matches!(request("-9223372036854775808").unwrap().params[1], super::Param::I64(i64::MIN)));
        assert!(matches!(request("18446744073709551615").unwrap().params[1], super::Param::U64(u64::MAX)));
        // Two `params` have no one answer, and the second would otherwise
        // bind its big number as the nearest double.
        let twice = r#"{"sql":"SELECT ?","params":[1],"params":[123456789012345678901234]}"#;
        assert!(super::parse_request(twice).err().unwrap().contains("duplicate field `params`"));
        // The fields in order, without their names, are not a request.
        assert_eq!(super::parse_request(r#"["SELECT 1",null,null,null,null]"#).err().unwrap(), "missing \"sql\"");
    }

    #[test]
    fn wrappers_and_local_mutations_require_connection_reset() {
        for sql in ["EXPLAIN ANALYZE BEGIN", "CALL f()", "EXECUTE s", "SET VARIABLE x=1",
            "CREATE TEMP TABLE t(x INTEGER)", "PREPARE s AS SELECT 1", "BEGIN"] {
            assert!(super::needs_connection_reset(sql), "{sql}");
        }
        assert!(!super::needs_connection_reset("INSERT INTO t VALUES (1)"));
    }

    #[test]
    fn the_tcp_door_is_one_ipv4_listener() {
        let doors = super::ipv4_loopback(0).unwrap();
        assert_eq!(doors.len(), 1);
        let justhttp::Listener::Tcp(listener) = &doors[0] else {
            panic!("the TCP door returned a unix listener");
        };
        assert_eq!(listener.local_addr().unwrap().ip(), std::net::Ipv4Addr::LOCALHOST);
    }

    /// The bug this design exists to prevent: a cancel decided for one
    /// statement must not fire on the next one to run on that connection.
    /// Without the id, "is something running?" is true in both cases and the
    /// interrupt lands on an innocent query.
    #[test]
    fn a_cancel_never_lands_on_the_next_statement() {
        let mut run = SlotRun::idle();
        run.begin(7, None);
        // Job 7 finishes before the cancel is decided.
        assert!(!run.end());
        // The next statement starts on the same connection.
        run.begin(8, None);
        // A cancel aimed at 7 arrives now. It must not touch 8, and it
        // stopped nothing, so it says so.
        assert_eq!(run.arm(Some(7)), Cancel::Nothing);
        assert!(!run.cancelled, "job 8 was marked cancelled by a cancel aimed at job 7");
        assert!(!run.end(), "job 8 reported itself cancelled");
    }

    /// A cancel that lands once its statement has finished, while the client
    /// is still reading the result, stopped nothing: it answers false and
    /// holds nothing for later.
    #[test]
    fn a_cancel_after_its_statement_finished_stops_nothing() {
        let mut run = SlotRun::idle();
        run.begin(5, None);
        assert!(!run.end());
        assert_eq!(run.arm(Some(5)), Cancel::Nothing);
        assert_eq!(run.pending, None);
        // One for a statement still to come is held for it.
        assert_eq!(run.arm(Some(6)), Cancel::Held);
        assert!(run.begin(6, None));
    }

    /// And the held cancel must not survive to ambush a later statement
    /// either — it named an id that will never run again.
    #[test]
    fn a_held_cancel_is_discarded_by_the_next_statement() {
        let mut run = SlotRun::idle();
        run.arm(Some(7));
        assert_eq!(run.pending, Some(7));
        assert!(!run.begin(9, None), "job 9 inherited a cancel meant for job 7");
        assert_eq!(run.pending, None);
    }

    /// The race the held cancel exists for: the client registers a query id,
    /// presses Stop, and the cancel arrives before the executor has picked the
    /// job up. The statement must not run.
    #[test]
    fn a_cancel_that_beats_its_statement_still_cancels_it() {
        let mut run = SlotRun::idle();
        assert_eq!(run.arm(Some(4)), Cancel::Held);
        assert!(run.begin(4, None), "job 4 ran despite being cancelled before it started");
    }

    #[test]
    fn cancelling_an_idle_slot_does_nothing() {
        let mut run = SlotRun::idle();
        assert_eq!(run.arm(None), Cancel::Nothing);
        assert!(!run.cancelled);
    }

    #[test]
    fn cancelling_whatever_is_running_does_not_need_an_id() {
        let mut run = SlotRun::idle();
        run.begin(3, None);
        assert_eq!(run.arm(None), Cancel::Fire);
        assert!(run.end(), "the statement did not report itself cancelled");
    }

    /// `end` clears the flag as well as reading it, so the next statement on
    /// this connection starts clean. A latched flag would report every
    /// subsequent query on that worker as cancelled.
    #[test]
    fn the_cancelled_flag_does_not_outlive_its_statement() {
        let mut run = SlotRun::idle();
        run.begin(1, None);
        run.arm(None);
        assert!(run.end());
        run.begin(2, None);
        assert!(!run.end());
    }

    #[test]
    fn a_deadline_only_expires_while_something_is_running() {
        let now = Instant::now();
        let past = now - Duration::from_secs(1);
        let mut run = SlotRun::idle();
        // Nothing running: a deadline in the past is not an expiry.
        run.deadline = Some(past);
        assert!(!run.expired(now));
        run.begin(1, Some(past));
        assert!(run.expired(now));
        run.begin(2, Some(now + Duration::from_secs(60)));
        assert!(!run.expired(now));
        // And a statement with no deadline never expires.
        run.begin(3, None);
        assert!(!run.expired(now + Duration::from_secs(86_400)));
    }

    /// Every rendering here is what duckdb_indexes() actually produced for
    /// these indexes, captured from a running engine rather than derived from
    /// the format description. Plain columns are bare, anything beyond a
    /// plain identifier is single-quoted with backslash escapes — including
    /// quoted identifiers, which keep their double quotes.
    #[test]
    fn recovers_index_columns_from_the_expressions_rendering() {
        assert_eq!(index_columns("[email]"), vec!["email"]);
        assert_eq!(index_columns("[title, user_id]"), vec!["title", "user_id"]);
        assert_eq!(index_columns(r#"['(lower("name"))']"#), vec![r#"(lower("name"))"#]);
        assert_eq!(
            index_columns(r#"['"a, b"', '"c\'d"', plain]"#),
            vec![r#""a, b""#, r#""c'd""#, "plain"]
        );
        assert_eq!(index_columns("[]"), Vec::<String>::new());
    }

    #[test]
    fn exact_catalog_counts_quote_every_relation() {
        let rows = vec![
            vec![serde_json::json!("main"), serde_json::json!("orders")],
            vec![serde_json::json!("odd schema"), serde_json::json!("say \"hi\"")],
        ];
        assert_eq!(
            catalog_count_sql(&rows).as_deref(),
            Some(
                "SELECT table_ordinal, row_count FROM (\
SELECT 0::UBIGINT AS table_ordinal, count(*)::UBIGINT AS row_count FROM \"main\".\"orders\" \
UNION ALL SELECT 1::UBIGINT AS table_ordinal, count(*)::UBIGINT AS row_count FROM \
\"odd schema\".\"say \"\"hi\"\"\") AS exact_counts ORDER BY table_ordinal"
            )
        );
        assert_eq!(catalog_count_sql(&[]), None);
    }

    /// `indexes[].columns` exists to be joined against `columns[].name`, so
    /// an identifier that needed quoting has to arrive unquoted — three of
    /// five names on an ordinary table need quoting. Anything
    /// that is not exactly one double-quoted identifier is an expression and
    /// is reported as one, so a computed index is never mistaken for a column
    /// with a peculiar name.
    #[test]
    fn index_parts_separate_columns_from_expressions() {
        let split = |rendering: &str| {
            let (mut cols, mut exprs) = (Vec::new(), Vec::new());
            for part in index_parts(rendering) {
                match part {
                    IndexPart::Column(c) => cols.push(c),
                    IndexPart::Expression(e) => exprs.push(e),
                }
            }
            (cols, exprs)
        };
        assert_eq!(split("[email]"), (vec!["email".to_string()], vec![]));
        assert_eq!(split("[title, user_id]"), (vec!["title".to_string(), "user_id".to_string()], vec![]));
        // quoted identifiers come back bare, so they join
        assert_eq!(split(r#"['"a b"']"#), (vec!["a b".to_string()], vec![]));
        assert_eq!(split(r#"['"c\'d"']"#), (vec!["c'd".to_string()], vec![]));
        assert_eq!(split(r#"['"é"']"#), (vec!["é".to_string()], vec![]));
        // a doubled quote inside an identifier survives as one quote
        assert_eq!(split(r#"['"a""b"']"#), (vec![r#"a"b"#.to_string()], vec![]));
        // an expression is never a column
        assert_eq!(
            split(r#"['(lower("name"))']"#),
            (vec![], vec![r#"(lower("name"))"#.to_string()])
        );
        // mixed, in order
        assert_eq!(
            split(r#"[plain, '"a b"', '(lower("n"))']"#),
            (vec!["plain".to_string(), "a b".to_string()], vec![r#"(lower("n"))"#.to_string()])
        );
    }

    #[test]
    fn backup_lifetime_renews_beyond_interactive_ceiling_but_cannot_revive() {
        use super::{LeaseLifetime, BACKUP_RENEWAL_TTL, LEASE_MAX_TTL, LEASE_IDLE_TTL};
        let start = Instant::now();
        let mut backup = LeaseLifetime::new(start, BACKUP_RENEWAL_TTL, true);
        let mut last = start;
        for seconds in (20..=600).step_by(20) {
            let now = start + Duration::from_secs(seconds);
            // No SQL activity: a long file scan does not consume the idle TTL.
            assert!(!backup.expired(now, start, false, LEASE_IDLE_TTL));
            assert!(backup.renew(now, &mut last, false, LEASE_IDLE_TTL));
        }
        assert_eq!(last, start, "a backup renewal moves its deadline, not its idle clock");
        let end = start + Duration::from_secs(660);
        assert!(backup.expired(end, start, true, LEASE_IDLE_TTL));
        assert!(!backup.renew(end, &mut last, false, LEASE_IDLE_TTL));
        assert!(!backup.renew(end + Duration::from_secs(1), &mut last, false, LEASE_IDLE_TTL));
        let interactive = LeaseLifetime::new(start, LEASE_MAX_TTL, false);
        assert!(interactive.expired(start + LEASE_MAX_TTL, start, true, LEASE_IDLE_TTL));
        assert!(interactive.expired(start + LEASE_IDLE_TTL, start, false, LEASE_IDLE_TTL));
        assert!(!interactive.expired(start + LEASE_IDLE_TTL, start, true, LEASE_IDLE_TTL));
    }

    #[test]
    fn a_query_id_in_a_path_is_percent_decoded() {
        use super::percent_decoded;
        assert_eq!(percent_decoded("cli-1-2"), "cli-1-2");
        assert_eq!(percent_decoded("a%20b%2Fc%3F%25"), "a b/c?%");
        assert_eq!(percent_decoded("caf%C3%A9"), "café");
        // A % that starts no escape is itself.
        assert_eq!(percent_decoded("100%"), "100%");
        assert_eq!(percent_decoded("%zz%4"), "%zz%4");
        assert_eq!(percent_decoded("%+1"), "%+1");
    }

    #[test]
    fn an_interactive_renewal_resets_the_idle_clock_and_leaves_the_ceiling() {
        use super::{LeaseLifetime, LEASE_MAX_TTL, LEASE_IDLE_TTL};
        let start = Instant::now();
        let mut lease = LeaseLifetime::new(start, LEASE_MAX_TTL, false);
        let mut last = start;
        // Renewed at a third of the idle limit, it outlives the idle limit
        // many times over, and only that.
        let step = LEASE_IDLE_TTL / 3;
        let mut now = start;
        while now + step < start + LEASE_MAX_TTL {
            now += step;
            assert!(lease.renew(now, &mut last, false, LEASE_IDLE_TTL), "{:?}", now - start);
            assert_eq!(last, now);
        }
        assert_eq!(lease.deadline, start + LEASE_MAX_TTL, "the ceiling stays");
        assert!(lease.expired(start + LEASE_MAX_TTL, last, false, LEASE_IDLE_TTL));
        assert!(!lease.renew(start + LEASE_MAX_TTL, &mut last, false, LEASE_IDLE_TTL));

        // One that idled out is not revived, though nothing reaped it yet;
        // one busy with a statement has no idle clock running.
        let mut idle = LeaseLifetime::new(start, LEASE_MAX_TTL, false);
        let mut last = start;
        assert!(!idle.renew(start + LEASE_IDLE_TTL, &mut last, false, LEASE_IDLE_TTL));
        assert_eq!(last, start);
        assert!(idle.renew(start + LEASE_IDLE_TTL, &mut last, true, LEASE_IDLE_TTL));
    }

    #[test]
    fn only_addresses_and_localhost_are_loopback_hosts() {
        for host in ["127.0.0.1:9495", "127.0.0.1", "localhost:80", "LOCALHOST", "[::1]:9495", "10.0.0.7:1"] {
            assert!(super::loopback_host(host), "{host}");
        }
        for host in ["attacker.example:9495", "live", "localhost.attacker.example", ""] {
            assert!(!super::loopback_host(host), "{host}");
        }
    }

    #[test]
    fn route_exists_matches_the_dispatch_table() {
        // Guards the hand-maintained coupling between `route_exists` and the
        // dispatch match in `handle`. Every real endpoint is a route; a known path
        // with the wrong method, and any unknown path, is not — so adding a
        // route to `handle` without updating `route_exists` fails here.
        //
        // The route list is not transcribed: it comes from the wire crate,
        // which is what clients read. A verb published there that harbor does
        // not serve is a 404 in the field and a failure here.
        fn method(m: &str) -> Method {
            match m {
                "GET" => Method::Get,
                "POST" => Method::Post,
                "DELETE" => Method::Delete,
                other => panic!("wire publishes an unmapped method: {other}"),
            }
        }
        let ids = [wire::endpoint::session("abc"), wire::endpoint::query("xyz"), wire::endpoint::session_renew("abc")];
        for r in wire::endpoint::FIXED.iter().chain(ids.iter()) {
            assert!(route_exists(&method(r.method), &r.path), "wire publishes {r}, harbor does not serve it");
        }
        let non_routes = [
            (Method::Post, "/sql/sessions//renew"),
            (Method::Post, "/sql/sessions/a/b/renew"),
            (Method::Get, "/sql/sessions/abc/renew"),
            (Method::Get, "/sql"),      // method matters
            (Method::Post, "/ready"),   // method matters
            (Method::Get, "/health"),   // never existed
            (Method::Get, "/"),
            (Method::Put, "/sql"),
        ];
        for (m, p) in &non_routes {
            assert!(!route_exists(m, p), "should NOT be a route: {m:?} {p}");
        }
        // The other spellings clients send: Rip's session route, the bare
        // collection path, and the DELETE verb.
        let aliases = [
            (Method::Post, "/sql/sessions/new"),
            (Method::Get, "/sessions"),
            (Method::Delete, "/shutdown"),
        ];
        for (m, p) in &aliases {
            assert!(route_exists(m, p), "an alias must stay served: {m:?} {p}");
        }
    }

    #[test]
    fn zstd_reader_round_trips_the_stream() {
        use std::io::Read;
        if crate::engine::engine().is_err() { return; }
        let conn = crate::engine::conn::open(std::path::Path::new(":memory:"), &[]).unwrap();
        let watch = super::Watch::new(None, super::SlotState::new(conn.interrupt_handle()), 0);
        // Chunks shaped like the executor's sends: several FLUSH_AT-sized
        // bodies, then a small tail, then the channel closes.
        let mut chunks: Vec<Vec<u8>> = Vec::new();
        let row = br#"{"type":"row","values":[123456789,"abc\n\"quoted\""]}"#;
        let mut big = Vec::new();
        while big.len() < 70 << 10 {
            big.extend_from_slice(row);
            big.push(b'\n');
        }
        chunks.push(big.clone());
        chunks.push(big);
        chunks.push(b"{\"type\":\"end\",\"rowCount\":2,\"timeMs\":1}\n".to_vec());
        let expected: Vec<u8> = chunks.iter().flatten().copied().collect();

        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(chunks.len());
        for c in &chunks {
            tx.send(c.clone()).unwrap();
        }
        drop(tx);

        let mut compressed = Vec::new();
        let mut reader = super::ZstdReader::new(rx, watch).unwrap();
        let mut first = [0u8; 16];
        let n = reader.read(&mut first).unwrap();
        compressed.extend_from_slice(&first[..n]);
        assert!(!reader.watch.ended, "a body still being read has not ended");
        reader.read_to_end(&mut compressed).unwrap();
        assert!(reader.watch.ended, "reading to the end is what marks it ended");
        assert!(compressed.len() < expected.len() / 3, "row envelopes should crush");

        let mut recovered = Vec::new();
        zstd::stream::Decoder::new(&compressed[..])
            .unwrap()
            .read_to_end(&mut recovered)
            .unwrap();
        assert_eq!(recovered, expected);
    }
}
