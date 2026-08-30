// harbor's library — the server engine: pool, leases, cancellation,
// timeouts, the NDJSON envelope, /sql /catalog /ready routing, and the
// SIGTERM → drain → CHECKPOINT shutdown path. The CLI (src/main.rs) is the
// only consumer; the two were one crate again once the loadable extension
// that justified a separate harbor-core retired (PLAN.md D5).
//
// This is the v0.9.1 extension's server code, moved verbatim. The extension
// glue (vtab table functions, entrypoint) stayed behind and retired with the
// extension (D5). The embedding host — `harbor serve` —
// opens the DuckDB Connection, hands it to `open_pool`, and calls
// `start`/`wait`/`stop`.

use std::{
    collections::HashMap,

    fmt::Write as _,
    io::Read,
    panic::AssertUnwindSafe,
    sync::{
        Arc, Condvar, Mutex, OnceLock, mpsc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use duckdb::{
    Connection, InterruptHandle, Result, params_from_iter,
    core::{LogicalTypeHandle, LogicalTypeId},
    types::Value,
};

use justhttp::{Header, Method, Request, Response, Server};

/// Re-exported so the CLI names the engine through the harbor crate —
/// one place owns the duckdb version pin.
pub use duckdb;

mod encode;
use encode::*;

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
//   GET  /ready   can this server answer a query? no auth
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

/// How many connections to open at load, when nothing says otherwise. Idle
/// connections are nearly free; not being able to open one later is not, and
/// "later" here means "ever" — see the note above.
///
/// The default covers the default six workers with ten left over for leases.
/// `HARBOR_POOL_SIZE` moves it, and has to, because this is the one number
/// that cannot be changed once the extension is loaded: a deployment that
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

/// Woken when the server stops, so `harbor_wait()` can block without polling.
static STOPPED: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());

// ---------------------------------------------------------------------------
// Cancellation
//
// A statement that has entered DuckDB does not come back until it is done.
// Harbor executes a small, bounded number of statements at once, so one query
// that runs forever is not a slow request — it is a worker permanently removed
// from service, and eight of them are the whole server. Three things can ask a
// statement to stop:
//
//   1. The client, by naming its own `queryId` and sending DELETE to it, or by
//      releasing a session whose statement is still running.
//   2. A deadline, when one was asked for.
//   3. The reaper, when a lease has outlived its TTL while busy. Before this
//      existed the reaper skipped busy leases, which meant the one lease that
//      most needed reclaiming — the one wedged inside a runaway statement —
//      was the one lease it could never take back.
//
// `duckdb_interrupt` is per connection, not per database (`InterruptHandle`
// wraps a `duckdb_connection`), so interrupting one statement cannot disturb
// another — including `harbor_wait()`, which runs on the caller's own
// connection and is not in the pool at all.
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
    /// When this worker began handling an HTTP request, whether or not that
    /// request has become a statement yet.
    ///
    /// A worker reading a request body has no job — `job` is still 0 — so to
    /// the probe thread it looked idle while being entirely stuck. That is not
    /// a corner case: every denial of service found against this server has
    /// worked by occupying workers BEFORE the statement starts, and the one
    /// thread whose purpose is staying reachable under saturation sat every
    /// one of them out because it was only ever looking at statements.
    request: Option<Instant>,
}

/// What a cancel request should do, decided from bookkeeping alone.
///
/// Split out from `SlotState` so the part that is easy to get wrong can be
/// tested without a database: this crate builds against DuckDB's loadable
/// extension API, so a `Connection` — and therefore an `InterruptHandle` —
/// cannot exist under `cargo test` at all.
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
    /// Claim this slot for `job`. Returns true when the statement was already
    /// cancelled before it began, in which case it must not run at all.
    fn begin(&mut self, job: u64, deadline: Option<Instant>) -> bool {
        self.job = job;
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
            // Named a statement this slot is not running. If it has not started
            // yet, hold the cancel for it; if it is long gone, `begin` discards
            // it on the next statement and nothing is interrupted.
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

    /// Interrupt the running statement if it is still `job` — or whatever is
    /// running, when `job` is None. Returns whether the cancel was accepted.
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
    // on the per-request path. (A runtime setenv no longer takes effect.)
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
            return Err(Refusal {
                status: 503,
                code: "unavailable",
                message: "harbor is not serving".to_string(),
            });
        };
        if queries.contains_key(id) {
            // Refuse rather than overwrite. Two live statements under one name
            // means a cancel is a coin flip, and silently replacing the first
            // would make the first uncancellable for as long as it runs.
            return Err(Refusal {
                status: 409,
                code: "query_id_in_use",
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
        // machinery exists to close (see SlotRun). A stale id is harmless: it
        // is held as pending and discarded when the next statement begins.
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
    deadline: Instant,
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

/// How long a lease may sit without a statement before it is reclaimed, and
/// the longest life it may ask for. The idle timeout is what actually protects
/// the checkpoint; the ceiling stops a client from asking for a lease that
/// outlives the reason it was granted.
const LEASE_IDLE_TTL: Duration = Duration::from_secs(30);
const LEASE_MAX_TTL: Duration = Duration::from_secs(300);
const REAP_INTERVAL: Duration = Duration::from_millis(500);

/// 18 bytes of CSPRNG, hex. Sessions are not a privilege boundary here — one
/// token admits every caller — so this is about never colliding and never
/// reusing, not about resisting an attacker who already has the token.
fn new_lease_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 18];
    rand::thread_rng().fill_bytes(&mut bytes);
    let mut out = String::with_capacity(36);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Run `ROLLBACK` on a lease connection and drain what it says. Called on
/// every path that returns a connection to the free list, so a lease can never
/// hand back a connection with a transaction still open on it.
///
/// Outside the registry lock, always: this blocks on the executor, and holding
/// the lock across it would serialise every other lease behind whatever this
/// connection is doing.
fn quiesce(conn: &LeaseConn) {
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), Refusal>>(1);
    let (body_tx, body_rx) = mpsc::sync_channel::<Vec<u8>>(BODY_QUEUE);
    let job = Job {
        sql: String::new(),
        params: Vec::new(),
        shape: Shape::Ndjson,
        id: next_job_id(),
        deadline: None,
        reset: true,
        ready: ready_tx,
        body: body_tx,
    };
    if conn.jobs.send(job).is_err() {
        return;
    }
    // Wait for it. A connection is not free until it is clean, and the caller
    // is about to put it back in the free list.
    let _ = ready_rx.recv();
    while body_rx.recv().is_ok() {}
}

/// Open a lease, or say why not. `Err` carries the status and body to send.
fn lease_open(requested_ttl: Option<Duration>) -> Result<(String, Duration, Duration), Refusal> {
    let mut guard = LEASES.lock().unwrap();
    let Some(leases) = guard.as_mut() else {
        return Err(Refusal {
            status: 503,
            code: "unavailable",
            message: "harbor is not serving".to_string(),
        });
    };
    if leases.total == 0 {
        return Err(Refusal {
            status: 503,
            code: "no_lease_connections",
            message: "this harbor has no connections left over for transactions: every one is a \
                      worker. Raise HARBOR_POOL_SIZE above the worker count, or lower workers."
                .to_string(),
        });
    }
    // Two clocks, and they answer different questions. The deadline caps how
    // long a lease may live at all, and the client may ask for less. The idle
    // timeout is harbor's own and is not negotiable: it is what reclaims a
    // client that stopped talking mid-transaction, which is the case that
    // blocks checkpoints, and letting a client raise it would let a client
    // disable it.
    let ttl = requested_ttl.unwrap_or(leases.max_ttl).min(leases.max_ttl);
    let idle_ttl = leases.idle_ttl;
    let Some(conn) = leases.free.pop() else {
        return Err(Refusal {
            status: 503,
            code: "no_lease_available",
            message: format!(
                "all {} transaction connections are in use. Retry, or raise HARBOR_POOL_SIZE.",
                leases.total
            ),
        });
    };
    let now = Instant::now();
    let deadline = now + ttl;
    let id = new_lease_id();
    leases.live.insert(
        id.clone(),
        Lease {
            conn,
            opened: now,
            last: now,
            deadline,
            statements: 0,
            in_transaction: false,
            busy: false,
            doomed: false,
        },
    );
    Ok((id, ttl, idle_ttl))
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
            Err(refusal) if refusal.code == "session_busy" && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(1));
            }
            other => return other,
        }
    }
}

fn try_lease_claim(id: &str) -> Result<(mpsc::SyncSender<Job>, Arc<SlotState>), Refusal> {
    let mut guard = LEASES.lock().unwrap();
    let Some(leases) = guard.as_mut() else {
        return Err(Refusal {
            status: 503,
            code: "unavailable",
            message: "harbor is not serving".to_string(),
        });
    };
    let Some(lease) = leases.live.get_mut(id) else {
        // Deliberately the same answer whether it never existed, was released,
        // or timed out: from the client's side those are one situation — the
        // transaction is gone and the work has to start again.
        return Err(Refusal {
            status: 404,
            code: "no_such_session",
            message: "no such session: it was released, timed out, or never existed. Open a new \
                      one and retry the transaction from the beginning."
                .to_string(),
        });
    };
    if lease.doomed {
        // The client asked for this lease's release; honoring new claims while
        // the cancel unwinds would let a "released" session that keeps sending
        // short statements stay busy at every reaper tick — held, with its
        // open transaction, forever. Same answer as absent: it is gone.
        return Err(Refusal {
            status: 404,
            code: "no_such_session",
            message: "no such session: it was released, timed out, or never existed. Open a new \
                      one and retry the transaction from the beginning."
                .to_string(),
        });
    }
    if lease.busy {
        return Err(Refusal {
            status: 409,
            code: "session_busy",
            message: "this session is already running a statement. A transaction is a sequence, \
                      not a pool; send its statements one after another."
                .to_string(),
        });
    }
    lease.busy = true;
    lease.last = Instant::now();
    Ok((lease.conn.jobs.clone(), Arc::clone(&lease.conn.state)))
}

/// Give a claim back, recording what the statement did to the transaction.
fn lease_settle(id: &str, sql: &str) {
    let mut guard = LEASES.lock().unwrap();
    let Some(leases) = guard.as_mut() else { return };
    let Some(lease) = leases.live.get_mut(id) else { return };
    lease.busy = false;
    lease.last = Instant::now();
    lease.statements += 1;
    if let Some(open) = transaction_effect(sql) {
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
}

impl Drop for Claim {
    fn drop(&mut self) {
        lease_settle(&self.id, &self.sql);
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
                let state = Arc::clone(&l.conn.state);
                // Outside the registry lock would be tidier, but `cancel` only
                // takes the slot's own lock and sets a flag through the C API,
                // so the nesting is one level deep and cannot cycle back here.
                state.cancel(None);
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
/// was running has come back. Before cancellation existed the reaper simply
/// skipped busy leases — which meant a lease wedged inside a runaway statement,
/// the one case where reclaiming actually matters, was the one case it could
/// never reclaim. The idle clock deliberately does not apply to a busy lease:
/// a statement that has been running for a minute is working, not idle.
fn lease_reap() {
    enum Action {
        Release(String),
        Cancel(Arc<SlotState>, u64),
    }
    let actions: Vec<Action> = {
        let guard = LEASES.lock().unwrap();
        let Some(leases) = guard.as_ref() else { return };
        let now = Instant::now();
        leases
            .live
            .iter()
            .filter_map(|(id, l)| match l.busy {
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
                true if now >= l.deadline || l.doomed => {
                    let job = l.conn.state.current_job();
                    (job != 0).then(|| Action::Cancel(Arc::clone(&l.conn.state), job))
                }
                true => None,
                false if l.doomed
                    || now >= l.deadline
                    || now.duration_since(l.last) >= leases.idle_ttl =>
                {
                    Some(Action::Release(id.clone()))
                }
                false => None,
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
        // waited on. It used to be left to the executor's own shutdown — its
        // channel is dropped below and `execute_jobs` rolls back on the way out
        // — but that only unwinds once the statement finishes, so a single long
        // query held the whole shutdown, and with it the CHECKPOINT that folds
        // the WAL.
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
    /// The saturation-proof lane: answers /ready when every worker is busy,
    /// and overflow requests on a borrowed lease connection. See probe_worker.
    probe: Option<JoinHandle<()>>,
    addr: String,
}

/// Open every connection harbor will need. Called once, from the extension
/// entrypoint, and only there — see the note above on why later is too late.
pub fn open_pool(con: Connection) -> Result<(), String> {
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

    for _ in 0..configured_pool_size() {
        pool.push(con.try_clone().map_err(|e| format!("harbor: {e}"))?);
    }
    // The handle has to be taken while the connection is still here, exactly
    // as `start()` does for the workers.
    *CONTROL_SLOT.lock().unwrap() = Some(Arc::new(SlotState {
        interrupt: con.interrupt_handle(),
        run: Mutex::new(SlotRun {
            job: 0,
            started: Instant::now(),
            pending: None,
            cancelled: false,
            deadline: None,
            request: None,
        }),
    }));
    *CONTROL.lock().unwrap() = Some(con);
    Ok(())
}

// ---------------------------------------------------------------------------
// start / stop / wait
// ---------------------------------------------------------------------------

/// Where the server listens. Unix sockets are the fleet's default face
/// (PLAN.md D3/D6); TCP remains for loopback and trusted-LAN use.
pub enum Listen {
    Tcp { bind: String, port: u16 },
    #[cfg(unix)]
    Unix(std::path::PathBuf),
}

pub fn start(
    listen: Listen,
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
        Listen::Tcp { bind, port } => Server::http((bind.as_str(), *port))
            .map_err(|e| format!("harbor: cannot bind {bind}:{port}: {e}")),
        #[cfg(unix)]
        Listen::Unix(path) => Server::http_unix(path.as_path())
            .map_err(|e| format!("harbor: cannot bind {}: {e}", path.display())),
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
    };
    *STARTED_AT.lock().unwrap() = Some(Instant::now());
    // Reset the process-global accumulators for this instance. harbor_serve
    // can follow a harbor_stop in the same process (the extension path), and
    // a request abandoned by the bounded-join shutdown may decrement
    // INFLIGHT_REQUESTS late — so a fresh instance must not inherit a nonzero
    // count (which would wedge quiet()/--idle-exit) or a stale readiness
    // verdict from its predecessor.
    INFLIGHT_REQUESTS.store(0, Ordering::SeqCst);
    *LAST_ACTIVITY.lock().unwrap() = None;
    *LAST_READY.lock().unwrap() = None;
    let server = Arc::new(server);
    let stop = Arc::new(AtomicBool::new(false));
    let token = Arc::new(token);

    // Every executor gets a slot before it gets a thread. The interrupt handle
    // has to be taken from the connection while it is still here — an executor
    // owns its connection for the life of the server and nothing else can
    // reach it afterwards.
    let mut slots: Vec<Arc<SlotState>> = Vec::with_capacity(workers + lease_conns.len());
    let new_slot = |conn: &Connection| {
        Arc::new(SlotState {
            interrupt: conn.interrupt_handle(),
            run: Mutex::new(SlotRun {
                job: 0,
                started: Instant::now(),
                pending: None,
                cancelled: false,
                deadline: None,
                request: None,
            }),
        })
    };

    let mut handles = Vec::with_capacity(workers);
    for (i, conn) in conns.into_iter().enumerate() {
        let server = Arc::clone(&server);
        let stop = Arc::clone(&stop);
        let token = Arc::clone(&token);
        let state = new_slot(&conn);
        slots.push(Arc::clone(&state));
        handles.push(
            thread::Builder::new()
                .name(format!("harbor-{i}"))
                .spawn(move || worker(server, stop, token, conn, state, log))
                .map_err(|e| e.to_string())?,
        );
    }

    // One executor per lease connection, each owning it for the life of the
    // server. A lease borrows the executor, not the thread: statements arrive
    // from whichever worker accepted the request and are answered here, which
    // is what keeps a transaction on one connection without taking a worker
    // out of the accept loop to babysit it.
    let mut lease_handles = Vec::with_capacity(lease_conns.len());
    let mut free = Vec::with_capacity(lease_conns.len());
    for (slot, conn) in lease_conns.into_iter().enumerate() {
        // Capacity 1 for the same reason as the worker executors: one job
        // outstanding by construction, so the slot only skips a double park.
        let (tx, rx) = mpsc::sync_channel::<Job>(1);
        let state = new_slot(&conn);
        slots.push(Arc::clone(&state));
        let exec_state = Arc::clone(&state);
        let handle = thread::Builder::new()
            .name(format!("harbor-lease-{slot}"))
            .spawn(move || Some(execute_jobs(conn, rx, true, exec_state)))
            .map_err(|e| e.to_string())?;
        lease_handles.push(handle);
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
    // blocks every checkpoint in between.
    // Unconditional now, where it used to be skipped when there were no lease
    // connections: it also enforces statement deadlines, and those apply to
    // workers, which always exist.
    let reaper = {
        let stop = Arc::clone(&stop);
        thread::Builder::new()
            .name("harbor-reaper".to_string())
            .spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    thread::sleep(REAP_INTERVAL);
                    cancel_expired();
                    lease_reap();
                }
            })
            .ok()
    };

    // One thread the fleet can always reach. Workers pair 1:1 with
    // connections and stream whole responses, so when every worker is busy an
    // accepted /ready sits in justhttp's queue until one frees — measured at
    // 5 seconds under a saturating analytical load — and a load balancer with
    // an ordinary timeout marks a busy-but-healthy berth dead precisely when
    // killing it hurts most. This thread never runs a statement of its own:
    // /ready is answered from the CONTROL connection, and anything else gets
    // a borrowed lease connection when one is free or an immediate honest 503
    // when the berth is truly saturated — shedding load instead of queueing
    // it invisibly.
    let probe = {
        let server = Arc::clone(&server);
        let stop = Arc::clone(&stop);
        let token = Arc::clone(&token);
        thread::Builder::new()
            .name("harbor-probe".to_string())
            .spawn(move || probe_worker(server, stop, token, log))
            .ok()
    };

    *STOPPED.0.lock().unwrap() = false;
    *running = Some(Running {
        server,
        stop,
        workers: handles,
        leases: lease_handles,
        reaper,
        probe,
        addr: addr.clone(),
    });
    Ok(addr)
}

pub fn stop() -> Result<String, String> {
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
    // harbor_serve has a pool to draw from. A panicked worker forfeits its
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
    if let Some(c) = CONTROL.lock().unwrap().as_ref()
        && let Err(e) = c.execute_batch("CHECKPOINT")
    {
        eprintln!(
            "harbor: shutdown CHECKPOINT failed ({e}); the WAL is intact and \
             will replay on next open (no data lost, slower restart)"
        );
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
            // absorbs a growing share of the traffic. `/ready` reports that
            // honestly now — it runs a real query, so a dead executor answers
            // 503 rather than a cheerful hardcoded 200 — but reporting it is
            // not enough: the worker still has to leave.
            Ok(Some(req)) => {
                if !handle(req, Some((&jobs_tx, &state)), token.as_ref().as_deref(), log) {
                    break;
                }
            }
            Ok(None) => continue,
            // The listener is gone — justhttp only surfaces an accept error
            // once it has decided the socket itself is unusable (transient
            // failures are retried there). This berth will never accept
            // another connection, and it used to leave without a word: the
            // process stayed alive, holding the database and the flock, while
            // every client saw connection-refused and every supervisor
            // watching the pid saw a healthy berth.
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
/// in-memory responses — this thread never streams and never borrows a
/// connection, so a client that stops reading can wedge a worker but not the
/// berth's last open door. Statements and /catalog get a fast honest 503
/// instead of queueing invisibly behind the analytics.
fn probe_worker(server: Arc<Server>, stop: Arc<AtomicBool>, token: Arc<Option<String>>, log: bool) {
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
        if !workers_wedged(Duration::from_millis(250)) {
            thread::sleep(Duration::from_millis(25));
            continue;
        }
        match server.recv_timeout(Duration::from_millis(100)) {
            Ok(Some(req)) => {
                let _ = handle(req, None, token.as_ref().as_deref(), log);
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
        let guard = CONTROL.lock().unwrap();
        let job = next_job_id();
        let _on_slot = slot.as_ref().map(|s| {
            s.begin(job, None);
            OnSlot { slot: s, done: false }
        });
        guard.as_ref().is_some_and(|c| c.execute_batch("SELECT 1").is_ok())
    };
    *LAST_READY.lock().unwrap() = Some((Instant::now(), ok));
    (true, respond_ready(req, ok, "not ready"))
}

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
/// "Occupied" is not "running a statement". It used to be, and that was the
/// blind spot behind every denial of service found here: a worker held in a
/// request body — draining one nobody read, or waiting on one dribbling in a
/// byte at a time — has no job, so six stuck workers read as six idle ones and
/// this returned false while the berth answered nothing at all. A worker is
/// occupied from the moment it picks up a request; whether that request ever
/// reaches DuckDB is a distinction the load balancer does not care about.
fn workers_wedged(min_age: Duration) -> bool {
    let slots = WORKER_SLOTS.lock().unwrap();
    !slots.is_empty()
        && slots.iter().all(|s| {
            let run = s.run.lock().unwrap();
            match run.job != 0 {
                true => run.started.elapsed() >= min_age,
                false => run.request.is_some_and(|t| t.elapsed() >= WEDGED_REQUEST_AGE),
            }
        })
}

/// The probe thread's answer to work it cannot take: immediate and honest,
/// instead of an invisible seat in the queue behind the analytics.
fn shed(req: Request) -> (bool, u16) {
    let _ = req.respond(error_response(503, "unavailable", "every worker is busy; retry shortly"));
    (true, 503)
}

// ---------------------------------------------------------------------------
// Berth identity (GET /info) and idle accounting (--idle-exit)
// ---------------------------------------------------------------------------

/// Identity document the embedding host sets before `start()`; GET /info
/// serves it with `uptimeMs` spliced in. The host owns the static fields
/// (name, database path, pid) because the core cannot know them.
/// Unset — as in the retiring extension — /info answers 404, which is
/// exactly the pre-fleet behavior clients use as a version probe.
static INFO: Mutex<Option<serde_json::Value>> = Mutex::new(None);
static STARTED_AT: Mutex<Option<Instant>> = Mutex::new(None);
/// The last moment a countable request began or finished. `/ready` and
/// `/info` do not count: a fleet `ls` probing liveness must not keep an
/// --idle-exit berth alive forever.
static LAST_ACTIVITY: Mutex<Option<Instant>> = Mutex::new(None);
/// Countable requests currently being served, statement and stream included.
/// This is what actually protects a long-running statement from --idle-exit:
/// the activity clock ticks only at request start and finish, so without this
/// a 5-minute COPY on an otherwise quiet berth would be "idle" at the 90s
/// mark and cancelled out from under its caller.
static INFLIGHT_REQUESTS: AtomicU64 = AtomicU64::new(0);
/// The workers' slots alone (SLOTS holds leases too), set at start(). The
/// probe thread reads these to decide whether the workers are wedged — every
/// one of them busy on a statement old enough to matter — which is the only
/// condition under which it takes requests at all.
static WORKER_SLOTS: Mutex<Vec<Arc<SlotState>>> = Mutex::new(Vec::new());

pub fn set_info(base: serde_json::Value) {
    *INFO.lock().unwrap() = Some(base);
}

fn touch_activity() {
    *LAST_ACTIVITY.lock().unwrap() = Some(Instant::now());
}

/// Milliseconds since the last countable request began or ended. The clock
/// resets at both edges of a request, but the guarantee that a statement
/// longer than the idle window is never idled out from under its caller
/// comes from `quiet()`, which refuses while any request is in flight.
pub fn idle_ms() -> u64 {
    let last = *LAST_ACTIVITY.lock().unwrap();
    let base = last.or(*STARTED_AT.lock().unwrap());
    base.map_or(0, |t| t.elapsed().as_millis() as u64)
}

/// True when nothing is held: no request mid-flight (statement or stream),
/// no live leases, no lease statement in flight. An --idle-exit berth may
/// leave only when this is true — an open transaction with no traffic is
/// still a claim on this berth, and so is a statement in its fifth minute.
///
/// A *doomed* lease is not a claim. It is one the server has already decided
/// to reclaim, waiting only for a cancelled statement to unwind, and a client
/// that asked to be released is not asking to be kept alive. Counting one is
/// how a single cancel that never lands turns `--idle-exit` off permanently:
/// `live` never empties, `quiet()` is false forever, and the berth outlives
/// every clock meant to retire it. That is not hypothetical — it is the shape
/// of a berth found still serving hours after its last session, deaf to the
/// idle timer that should have ended it.
pub fn quiet() -> bool {
    if INFLIGHT_REQUESTS.load(Ordering::SeqCst) != 0 {
        return false;
    }
    match LEASES.lock().unwrap().as_ref() {
        Some(l) => l.live.values().all(|lease| lease.doomed) && l.inflight == 0,
        None => false,
    }
}

fn run_info(req: Request) -> (bool, u16) {
    let info = INFO.lock().unwrap().clone();
    match info {
        Some(mut v) => {
            if let Some(obj) = v.as_object_mut() {
                let up = STARTED_AT.lock().unwrap().map_or(0, |t| t.elapsed().as_millis() as u64);
                obj.insert("uptimeMs".to_string(), serde_json::Value::from(up));
            }
            let _ = req.respond(json_response(200, &v.to_string()));
            (true, 200)
        }
        None => {
            let _ = req.respond(error_response(404, "not_found", "no such endpoint"));
            (true, 404)
        }
    }
}

/// One request. `exec` is the accepting thread's executor — its jobs channel
/// and cancellation slot. The probe thread passes None: it owns no
/// connection, so the arms that stream (/sql, /catalog, the workers' /ready)
/// shed load with an immediate 503 instead, and every control-plane verb —
/// session open/release, query cancel, /sessions, /info — works exactly as
/// it does on a worker, because none of them touch an executor.
fn handle(
    mut req: Request,
    exec: Option<(&mpsc::SyncSender<Job>, &Arc<SlotState>)>,
    token: Option<&str>,
    log: bool,
) -> bool {
    let path = req.url().split('?').next().unwrap_or("/").to_string();
    let method = req.method().clone();
    let countable = path != "/ready" && path != "/info";
    // A drop guard, not a bare inc/dec, so a panic anywhere in this function
    // (a poisoned global lock, a panic inside response construction) cannot
    // leak the count and wedge quiet()/--idle-exit — the same RAII discipline
    // as Claim/Cancellable/OnSlot. Touches the activity clock on both edges.
    let _inflight = countable.then(InFlight::enter);
    // Only a worker marks a slot: the probe thread owns no connection and is
    // never what `workers_wedged` is asking about.
    let _occupied = exec.map(|(_, slot)| OnRequest::enter(slot));

    // Only when logging. A clock read and a peer-address format are small, but
    // they are paid on every request by every caller, including the ones that
    // asked for a query endpoint and nothing else.
    let started = log.then(Instant::now);
    let peer = match log {
        true => req.remote_addr().map_or_else(|| "-".to_string(), |a| a.ip().to_string()),
        false => String::new(),
    };
    // Cleared here so the reason logged below is this request's, never a
    // previous one's left on this worker thread.
    LAST_REASON.with(|c| c.set(""));

    // Two gates before any routing, in this order.
    //
    // The declared length first: justhttp drains an undelivered body when a
    // request is dropped — with a single `vec![0; remaining]` — and it does so
    // for EVERY response path, 401s and 404s included. `take()` bounds what
    // harbor buffers but not what the client may declare, and the declared
    // length is attacker-chosen; a request declaring 1 GB and sending 9 bytes
    // used to cost this process a 1 GB zeroed allocation, unauthenticated.
    // Refusing here, before anything else can respond, means the allocation
    // never happens on any path.
    //
    // Then the token: /ready is the one unauthenticated route (a load balancer
    // should not need a credential to learn up-or-down, and the answer reveals
    // nothing else), so the property "everything except /ready requires the
    // token" is enforced once, here, instead of being re-asserted arm by arm —
    // where the arm someone adds next year would forget it.
    //
    // Each arm reports the status it sent, so the log line below is written in
    // one place instead of at every `respond` call. The SQL text is not
    // logged: it is the request body, it can be enormous, and on this endpoint
    // it is as likely to hold customer data as anything else in the database.
    let (keep_going, status) = if let Some(n) = req.body_length().filter(|n| *n > MAX_BODY) {
        let _ = req.respond(error_response(
            413,
            "body_too_large",
            &format!("body is {n} bytes; the limit is {MAX_BODY}"),
        ));
        (true, 413)
    } else if path != "/ready" && !authorized(&req, token) {
        // Unknown paths stay 404 even unauthenticated — the contract the
        // clients pinned long before this gate was hoisted. Known endpoints
        // answer 401 so a caller with a bad token learns which problem it has.
        if route_exists(&method, &path) {
            let _ = req.respond(error_response(401, "unauthorized", "missing or invalid bearer token"));
            (true, 401)
        } else {
            let _ = req.respond(error_response(404, "not_found", "no such endpoint"));
            (true, 404)
        }
    } else {
        match (&method, path.as_str()) {
            // Readiness is unauthenticated on purpose (see the gate above).
            // Workers answer it down the full query path; the probe thread —
            // the one still listening when every worker is saturated —
            // answers from the CONTROL connection instead of queueing.
            (Method::Get, "/ready") => match exec {
                Some((jobs, _)) => run_ready(req, jobs),
                None => run_ready_control(req),
            },
            // Open a transaction lease. It consumes a connection, which is the
            // scarcest thing here.
            (Method::Post, "/sql/sessions/new") => run_session_open(req),
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
                let id = p.trim_start_matches("/sql/sessions/").to_string();
                let body = match lease_release(&id) {
                    Released::Yes => r#"{"released":true}"#.to_string(),
                    Released::No => r#"{"released":false}"#.to_string(),
                    Released::Cancelling => r#"{"released":false,"cancelling":true}"#.to_string(),
                };
                let _ = req.respond(json_response(200, &body));
                (true, 200)
            }
            // Stop a statement the client named when it sent it. Idempotent and
            // deliberately unexciting: cancelling something that already
            // finished is `false`, not an error, because by the time a Stop
            // button is pressed the query it refers to may well be over.
            (Method::Delete, p) if p.starts_with("/sql/queries/") => {
                let id = p.trim_start_matches("/sql/queries/").to_string();
                let cancelled = cancel_query(&id);
                let _ = req.respond(json_response(200, &format!(r#"{{"cancelled":{cancelled}}}"#)));
                (true, 200)
            }
            // What is holding a connection, and for how long. The question an
            // operator asks when everything is suddenly waiting, and the reason
            // this exists at all: a pool you cannot see into is a pool you
            // debug by guessing.
            (Method::Get, "/sessions") => {
                let _ = req.respond(json_response(200, &sessions_report()));
                (true, 200)
            }
            // A Pilot sitting at its prompt is active even though it has no
            // SQL request in flight. This route deliberately does no engine
            // work and holds no lease; being a countable request is its whole
            // job. When Pilot disappears, the pulses disappear too and the
            // ordinary idle-exit clock reaps the berth.
            (Method::Get, "/keepalive") => {
                let _ = req.respond(json_response(200, r#"{"alive":true}"#));
                (true, 200)
            }
            // Fleet shutdown is authenticated and returns before the drain
            // begins. Running stop() on a fresh thread matters: this handler
            // is itself one of the workers stop() waits to join.
            (Method::Delete, "/shutdown") => {
                let _ = req.respond(json_response(202, r#"{"stopping":true}"#));
                let _ = thread::Builder::new()
                    .name("harbor-shutdown".to_string())
                    .spawn(|| {
                        let _ = stop();
                    });
                (false, 202)
            }
            // Berth identity: who serves here, which engine, since when. Auth
            // required — it names filesystem paths and pids. 404 when the host
            // never set one, which is also what pre-fleet servers answer:
            // absence is the version probe.
            (Method::Get, "/info") => run_info(req),
            // The whole schema — tables, columns, keys, indexes, sequences — in
            // one call, in one shape. It lives here so a migration differ asks
            // a single question instead of five, and so the answer never
            // depends on which DuckDB this binary links: the queries below use
            // whatever the engine's catalog provides, and version differences
            // die in this process rather than in every client.
            (Method::Get, "/catalog") => match exec {
                Some((jobs, _)) => run_catalog(req, jobs),
                None => shed(req),
            },
            (Method::Post, "/sql") => match exec {
                Some((jobs, state)) => {
                    // Pre-size from the declared length, capped: the value is
                    // client-chosen (up to MAX_BODY), so trust it only up to
                    // 16KB and let anything larger grow normally.
                    let mut body =
                        String::with_capacity(req.body_length().unwrap_or(0).min(16 * 1024));
                    // Not only UTF-8 any more: the socket carries a read
                    // timeout, so a client that stops mid-body lands here too.
                    if req.as_reader().take(MAX_BODY as u64).read_to_string(&mut body).is_err() {
                        let _ = req.respond(error_response(
                            400,
                            "bad_request",
                            "the request body could not be read: it is not valid UTF-8, or it \
                             stopped arriving",
                        ));
                        (true, 400)
                    } else {
                        run_sql(req, jobs, state, &body)
                    }
                }
                None => shed(req),
            },
            _ => {
                let _ = req.respond(error_response(404, "not_found", "no such endpoint"));
                (true, 404)
            }
        }
    };

    // After respond(), not before: justhttp writes the body from the reader
    // inside that call, so for a streamed result this elapsed time covers the
    // whole query and the whole transfer rather than just the headers.
    if let Some(t) = started {
        // On a failure, name why: the refusal code turns an unexplained spike
        // of 4xx/5xx in the berth's own log into something diagnosable. The SQL
        // and the message still stay out of the log (privacy, and the message
        // can be large); the code is a fixed vocabulary and is enough to act on.
        let reason = LAST_REASON.with(|c| c.get());
        let reason = if status >= 400 && !reason.is_empty() {
            format!(" {reason}")
        } else {
            String::new()
        };
        eprintln!(
            "harbor: {} {peer} {} {path} {status}{reason} {}ms",
            utc_now(),
            method.as_str(),
            t.elapsed().as_millis()
        );
    }
    // The finish counts too (see idle_ms): a long statement resets the idle
    // clock when it completes, not only when it began. `_inflight` drops on
    // return — after respond(), so the whole stream was this request.
    keep_going
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
        slot.run.lock().unwrap().request = Some(Instant::now());
        OnRequest { slot }
    }
}

impl Drop for OnRequest<'_> {
    fn drop(&mut self) {
        self.slot.run.lock().unwrap().request = None;
    }
}

/// Marks a countable request in flight for the life of the value: increments
/// on `enter`, decrements on drop, and touches the activity clock at both
/// edges. Dropping on every path (return, `?`, panic) is the point.
struct InFlight;

impl InFlight {
    fn enter() -> Self {
        touch_activity();
        INFLIGHT_REQUESTS.fetch_add(1, Ordering::SeqCst);
        InFlight
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        // Saturating, so a request abandoned by one server instance whose
        // guard drops after the next instance reset the counter to 0 can
        // never underflow it (u64 wrap would leave quiet() false forever).
        let _ = INFLIGHT_REQUESTS
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| Some(n.saturating_sub(1)));
        touch_activity();
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


/// The route list, method included, for the auth gate above: it must agree
/// with the match in `handle` so 401-vs-404 answers stay truthful. GET /sql
/// is not a route (the method matters), and unknown paths are 404 with or
/// without a token.
fn route_exists(method: &Method, path: &str) -> bool {
    matches!(
        (method, path),
        (Method::Get, "/ready" | "/sessions" | "/info" | "/catalog" | "/keepalive")
            | (Method::Delete, "/shutdown")
            | (Method::Post, "/sql" | "/sql/sessions/new")
    ) || (*method == Method::Delete
        && (path.starts_with("/sql/sessions/") || path.starts_with("/sql/queries/")))
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
    let mut v: serde_json::Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
    // take() moves the String serde already built instead of copying it
    let sql = match v.get_mut("sql").map(serde_json::Value::take) {
        Some(serde_json::Value::String(s)) => s,
        _ => return Err("missing \"sql\"".to_string()),
    };
    if sql.trim().is_empty() {
        return Err("\"sql\" is empty".to_string());
    }
    let params = match v.get("params") {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(serde_json::Value::Array(a)) => a.iter().map(json_to_duckdb).collect::<Result<_, _>>()?,
        Some(_) => return Err("\"params\" must be an array".to_string()),
    };
    let session = match v.get("sessionId") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(id)) if !id.is_empty() => Some(id.clone()),
        Some(_) => return Err("\"sessionId\" must be a non-empty string".to_string()),
    };
    let query = match v.get("queryId") {
        None | Some(serde_json::Value::Null) => None,
        // Bounded, because it becomes a key in a map that lives as long as the
        // server and is written by anyone holding the token.
        Some(serde_json::Value::String(id)) if !id.is_empty() && id.len() <= 128 => {
            Some(id.clone())
        }
        Some(_) => {
            return Err("\"queryId\" must be a non-empty string of at most 128 characters"
                .to_string());
        }
    };
    // The operator's `--statement-timeout` is a hard ceiling, not just a
    // default: a request may ask for *less*, but not for more, and `0` ("no
    // limit") is bounded by it too. Without the clamp, any token holder could
    // send `timeoutMs:0` and pin a worker indefinitely — defeating the very
    // knob a `--sealed` deployment leans on. When no cap is configured the cap
    // is `None`, so the historical behaviour (0 = unlimited, N = exactly N) is
    // unchanged.
    let cap = configured_statement_timeout();
    let timeout = match v.get("timeoutMs") {
        None | Some(serde_json::Value::Null) => cap,
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
/// A byte that can appear inside a DuckDB identifier. `$` is one of them,
/// which is why a `$` after one does not open a dollar-quote; bytes >= 0x80
/// are UTF-8 continuation or lead bytes and belong to whatever identifier
/// they are part of.
fn is_ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

fn ensure_single_statement(sql: &str) -> Result<(), String> {
    let b = sql.as_bytes();
    let mut i = 0;

    while i < b.len() {
        match b[i] {
            b'-' if b.get(i + 1) == Some(&b'-') => {
                // Both terminators, and the second one is not a nicety.
                // DuckDB inherits Postgres's lexer, which ends a `--` comment
                // at CR as well as LF. Scanning for LF alone read
                // `SELECT 1 --\r; DROP TABLE orders` as one statement with a
                // comment on the end, while the engine read two — and
                // `duckdb-rs` runs everything but the last during `prepare`,
                // so the DROP landed before a row was ever fetched. That is
                // this function's one job, defeated by one byte. (CR is the
                // only divergence: VT, FF, NEL, U+2028 and U+2029 do not end a
                // comment for either of us — verified against the engine.)
                i = b[i..]
                    .iter()
                    .position(|&c| c == b'\n' || c == b'\r')
                    .map_or(b.len(), |p| i + p + 1);
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
                //
                // Only when the `$` actually opens something. DuckDB allows
                // `$` *inside* an identifier, so `a$b$c` is one identifier and
                // not a dollar-quote — but this scanner read the `$b$` as an
                // opener, hunted for a closing `$b$` that was never coming,
                // and swallowed the rest of the input as string content.
                // Everything after it, terminator included, then looked like
                // data: `SELECT 1 a$b$c; DROP TABLE orders` was accepted as a
                // single statement and the DROP ran during `prepare`. Same
                // shape as the CR-comment hole, same consequence.
                //
                // pilot's scanner already guarded this (scan.rs, `prev_ident`)
                // — the two must agree, and the security lexer was the one
                // that was wrong.
                let opens = i == 0 || !is_ident_byte(b[i - 1]);
                let tag_end = b[i + 1..]
                    .iter()
                    .position(|&c| !(c.is_ascii_alphanumeric() || c == b'_'))
                    .map(|p| i + 1 + p);
                match tag_end {
                    // A tag may not start with a digit: `$1$` is a bind
                    // parameter to DuckDB, not the opening of a string. Reading
                    // it as one would let `$1$; DROP TABLE t; $1$` hide a
                    // terminator the two lexers disagree about.
                    Some(end) if opens && b[end] == b'$' && !b[i + 1].is_ascii_digit() => {
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

/// `Accept: application/json` asks for the whole result as one document, the
/// way harbor v1 did. Anything else — no header, `*/*`, `application/x-ndjson`
/// — streams.
///
/// A header naming both wins for NDJSON: it is the shape that cannot fail on
/// size, so it is the safe reading of an ambiguous request. (`application/json`
/// is not a substring of `application/x-ndjson`, so a plain `contains` is not
/// fooled by the streaming type.)
fn wants_one_shot(req: &Request) -> bool {
    // case-insensitive substring scan, allocation-free (same semantics as
    // the lowercase-then-contains it replaced)
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
        Self { status: 400, code: "sql_error", message: message.into() }
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
            code: "cancelled",
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
    params: Vec<Value>,
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

/// How many body batches may be in flight before the query has to wait.
const BODY_QUEUE: usize = 4;

/// How long a `/ready` verdict is served before another query is run to
/// refresh it.
///
/// Readiness has to run a real query to mean anything, and this endpoint takes
/// no credential, so without a cache anyone who can reach the port has a free
/// way to make the server work — on the same bounded pool that serves paying
/// traffic. One second bounds that to one query per second no matter how often
/// it is asked, which is well inside what any prober polls at. The cost is that
/// a database that wedges is reported ready for up to a second longer; a probe
/// interval is measured in seconds, so nothing observes the difference.
const READY_MAX_AGE: Duration = Duration::from_secs(1);

/// The last verdict and when it was taken. Failures are cached too — a server
/// that cannot answer is exactly the one that must not be asked N more times a
/// second.
static LAST_READY: Mutex<Option<(Instant, bool)>> = Mutex::new(None);

/// `POST /sql/sessions/new` — take a connection out of the pool and hold it.
///
/// The body may ask for a lifetime (`{"ttlMs": N}`); harbor caps it at its own
/// maximum and answers with the one it actually granted, alongside the idle
/// timeout it enforces regardless. Both sides then know when the lease dies,
/// which beats the client discovering it at COMMIT — the point at which the
/// work is already done and cannot be redone cheaply.
fn run_session_open(mut req: Request) -> (bool, u16) {
    let mut body = String::new();
    if req.as_reader().take(MAX_BODY as u64).read_to_string(&mut body).is_err() {
        let _ = req.respond(error_response(
            400,
            "bad_request",
            "the request body could not be read: it is not valid UTF-8, or it stopped arriving",
        ));
        return (true, 400);
    }
    let requested = match body.trim().is_empty() {
        true => None,
        false => match serde_json::from_str::<serde_json::Value>(&body) {
            Ok(v) => match v.get("ttlMs") {
                None | Some(serde_json::Value::Null) => None,
                Some(n) => match n.as_u64() {
                    Some(ms) if ms > 0 => Some(Duration::from_millis(ms)),
                    _ => {
                        let _ = req.respond(error_response(
                            400,
                            "bad_request",
                            "\"ttlMs\" must be a positive integer",
                        ));
                        return (true, 400);
                    }
                },
            },
            Err(e) => {
                let _ = req.respond(error_response(400, "bad_request", &e.to_string()));
                return (true, 400);
            }
        },
    };

    match lease_open(requested) {
        Ok((id, ttl, idle_ttl)) => {
            let body = format!(
                r#"{{"sessionId":"{}","ttlMs":{},"idleTtlMs":{}}}"#,
                id,
                ttl.as_millis(),
                idle_ttl.as_millis()
            );
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
            if refusal.code == "no_lease_available" {
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
            r#"{{"sessionId":"{}","slot":{},"ageMs":{},"idleMs":{},"expiresInMs":{},"statements":{},"inTransaction":{},"busy":{}}}"#,
            id,
            lease.conn.slot,
            now.duration_since(lease.opened).as_millis(),
            now.duration_since(lease.last).as_millis(),
            lease.deadline.saturating_duration_since(now).as_millis(),
            lease.statements,
            lease.in_transaction,
            lease.busy
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
/// dozen rows, nowhere near the size that shape refuses.
fn catalog_rows(
    jobs: &mpsc::SyncSender<Job>,
    sql: &str,
) -> Result<Vec<Vec<serde_json::Value>>, CatalogFailure> {
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), Refusal>>(1);
    let (body_tx, body_rx) = mpsc::sync_channel::<Vec<u8>>(BODY_QUEUE);
    let job = Job {
        sql: sql.to_string(),
        params: Vec::new(),
        shape: Shape::Json,
        id: next_job_id(),
        // The deployment default applies here as it does to any statement.
        deadline: configured_statement_timeout().map(|t| Instant::now() + t),
        reset: false,
        ready: ready_tx,
        body: body_tx,
    };
    if jobs.send(job).is_err() {
        return Err(CatalogFailure::Gone);
    }
    let verdict = ready_rx.recv();
    // Drain rather than drop, for the same reason `run_ready` does: a dropped
    // receiver reads as a client that hung up mid-stream and costs a rollback.
    let mut document = Vec::new();
    while let Ok(chunk) = body_rx.recv() {
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
                code: "internal",
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
            let _ = req.respond(error_response(503, "unavailable", "harbor is shutting down"));
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

fn cell_u64(row: &[serde_json::Value], i: usize) -> u64 {
    // The executor's integer policy quotes a value past JSON's exact range,
    // so a cell can arrive as either a number or its decimal string.
    row.get(i)
        .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
        .unwrap_or(0)
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

struct CatalogColumn {
    name: String,
    ty: String,
    not_null: bool,
    default: Option<String>,
    primary: bool,
}

struct CatalogIndex {
    name: String,
    columns: Vec<String>,
    expressions: Vec<String>,
    unique: bool,
}

struct CatalogFk {
    columns: Vec<String>,
    ref_table: String,
    ref_schema: String,
    ref_columns: Vec<String>,
}

struct CatalogTable {
    schema: String,
    name: String,
    estimated_rows: u64,
    ddl: Option<String>,
    columns: Vec<CatalogColumn>,
    primary_key: Vec<String>,
    unique_constraints: Vec<Vec<String>>,
    indexes: Vec<CatalogIndex>,
    foreign_keys: Vec<CatalogFk>,
}

/// The document's opening run, shared by both styles: versions and sizes,
/// with the object left open for the style's own `tables` emission.
fn catalog_header(duckdb_version: &str) -> String {
    let (database_size, wal_size) = database_disk_sizes();
    let mut out = String::from("{\"harborVersion\":");
    push_json_string(&mut out, env!("CARGO_PKG_VERSION"));
    out.push_str(",\"duckdbVersion\":");
    push_json_string(&mut out, duckdb_version);
    out.push_str(",\"databaseSizeBytes\":");
    match database_size {
        Some(bytes) => out.push_str(&bytes.to_string()),
        None => out.push_str("null"),
    }
    out.push_str(",\"walSizeBytes\":");
    match wal_size {
        Some(bytes) => out.push_str(&bytes.to_string()),
        None => out.push_str("null"),
    }
    out
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
/// exists and how big; full adds how everything is built.
enum CatalogStyle {
    Full,
    Lite,
}

/// `?style=` from the request url. No query and no `style` mean full. An
/// unknown *value* is refused loudly — a style the caller asked for and did
/// not get would corrupt silently — while unknown *parameters* pass, because
/// that tolerance is exactly what lets a 0.17 client send `style=lite` to a
/// 0.16 server and still get a correct (full) answer.
fn catalog_style(url: &str) -> Result<CatalogStyle, String> {
    let Some(query) = url.splitn(2, '?').nth(1) else { return Ok(CatalogStyle::Full) };
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
/// The document also carries what a browsing client otherwise dials three
/// more queries for: each table's `estimatedRows` (the engine's cardinality
/// estimate — a sidebar figure, not a COUNT(*)), each table's `ddl` as the
/// engine renders it, and `databaseSizeBytes`/`walSizeBytes` statted from
/// the served file by the one process sitting next to it — exact bytes,
/// never the engine's pretty-printed strings, and null for a berth serving
/// no file.
///
/// `?style=lite` answers the inventory alone: the versions, the sizes, and
/// each table as name, schema, and `estimatedRows` — enough to draw a
/// database list without paying for columns, constraints, indexes, DDL, or
/// sequences, in queries here or in bytes on the wire. It is the same
/// document family at lower fidelity, not a second contract: a field a
/// style omits is absent, never differently shaped.
///
/// Ordering is part of the contract: tables by (schema, name), columns in
/// ordinal position, indexes and sequences by name, unique constraints by
/// their column lists, foreign keys by their referenced table and column
/// lists. A stable database answers with byte-identical output.
fn run_catalog(req: Request, jobs: &mpsc::SyncSender<Job>) -> (bool, u16) {
    let style = match catalog_style(req.url()) {
        Ok(style) => style,
        Err(message) => {
            let _ = req.respond(error_response(400, "bad_request", &message));
            return (true, 400);
        }
    };
    // System and temp catalogs are excluded by anchoring every query to the
    // served database: `system` and `temp` are separate databases, so
    // current_database() never matches them.
    let version_rows = match catalog_rows(jobs, "SELECT library_version FROM pragma_version()") {
        Ok(rows) => rows,
        Err(failure) => return catalog_refuse(req, failure),
    };
    let table_rows = match catalog_rows(
        jobs,
        "SELECT schema_name, table_name, estimated_size, sql FROM duckdb_tables() \
         WHERE database_name = current_database() AND NOT internal AND NOT temporary \
         ORDER BY schema_name, table_name",
    ) {
        Ok(rows) => rows,
        Err(failure) => return catalog_refuse(req, failure),
    };
    let duckdb_version = version_rows.first().map(|r| cell_str(r, 0)).unwrap_or_default();

    // The lite style stops here: everything it answers is already in hand,
    // and the four shape queries below never run.
    if let CatalogStyle::Lite = style {
        let mut out = catalog_header(&duckdb_version);
        out.push_str(",\"tables\":[");
        for (i, row) in table_rows.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("{\"name\":");
            push_json_string(&mut out, &cell_str(row, 1));
            out.push_str(",\"schema\":");
            push_json_string(&mut out, &cell_str(row, 0));
            out.push_str(",\"estimatedRows\":");
            out.push_str(&cell_u64(row, 2).to_string());
            out.push('}');
        }
        out.push_str("]}");
        let _ = req.respond(json_response(200, &out));
        return (true, 200);
    }

    let column_rows = match catalog_rows(
        jobs,
        "SELECT schema_name, table_name, column_name, data_type, is_nullable, column_default \
         FROM duckdb_columns() \
         WHERE database_name = current_database() AND NOT internal \
         ORDER BY schema_name, table_name, column_index",
    ) {
        Ok(rows) => rows,
        Err(failure) => return catalog_refuse(req, failure),
    };
    let constraint_rows = match catalog_rows(
        jobs,
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
        jobs,
        "SELECT schema_name, table_name, index_name, is_unique, expressions \
         FROM duckdb_indexes() \
         WHERE database_name = current_database() \
         ORDER BY schema_name, table_name, index_name",
    ) {
        Ok(rows) => rows,
        Err(failure) => return catalog_refuse(req, failure),
    };
    let sequence_rows = match catalog_rows(
        jobs,
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
    let mut tables: Vec<CatalogTable> = Vec::new();
    let mut index_of: HashMap<(String, String), usize> = HashMap::new();
    for row in &table_rows {
        let schema = cell_str(row, 0);
        let name = cell_str(row, 1);
        index_of.insert((schema.clone(), name.clone()), tables.len());
        tables.push(CatalogTable {
            schema,
            name,
            estimated_rows: cell_u64(row, 2),
            ddl: cell_opt_str(row, 3),
            columns: Vec::new(),
            primary_key: Vec::new(),
            unique_constraints: Vec::new(),
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        });
    }
    for row in &column_rows {
        let Some(&t) = index_of.get(&(cell_str(row, 0), cell_str(row, 1))) else { continue };
        tables[t].columns.push(CatalogColumn {
            name: cell_str(row, 2),
            ty: cell_str(row, 3),
            not_null: !cell_bool(row, 4),
            default: cell_opt_str(row, 5),
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
                tables[t].unique_constraints.push(columns);
            }
            "FOREIGN KEY" => {
                let ref_schema = tables[t].schema.clone();
                tables[t].foreign_keys.push(CatalogFk {
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
        tables[t].indexes.push(CatalogIndex {
            name: cell_str(row, 2),
            columns,
            expressions,
            unique: cell_bool(row, 3),
        });
    }
    for table in tables.iter_mut() {
        // A unique constraint has no name in this shape either, so the same
        // rule: pin its position to its column list, never to storage order.
        table.unique_constraints.sort();
        // A foreign key has no name in this shape, so its position cannot be
        // inherited from catalog storage order; pin it to what the entry says.
        table.foreign_keys.sort_by(|a, b| {
            (&a.ref_table, &a.columns, &a.ref_columns).cmp(&(&b.ref_table, &b.columns, &b.ref_columns))
        });
    }

    let mut out = catalog_header(&duckdb_version);
    out.push_str(",\"tables\":[");
    for (i, table) in tables.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"name\":");
        push_json_string(&mut out, &table.name);
        out.push_str(",\"schema\":");
        push_json_string(&mut out, &table.schema);
        out.push_str(",\"estimatedRows\":");
        out.push_str(&table.estimated_rows.to_string());
        out.push_str(",\"columns\":[");
        for (j, column) in table.columns.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str("{\"name\":");
            push_json_string(&mut out, &column.name);
            out.push_str(",\"type\":");
            push_json_string(&mut out, &column.ty);
            out.push_str(",\"notNull\":");
            out.push_str(if column.not_null { "true" } else { "false" });
            out.push_str(",\"default\":");
            match &column.default {
                Some(expression) => push_json_string(&mut out, expression),
                None => out.push_str("null"),
            }
            out.push_str(",\"primary\":");
            out.push_str(if column.primary { "true" } else { "false" });
            out.push('}');
        }
        out.push_str("],\"primaryKey\":[");
        for (j, name) in table.primary_key.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            push_json_string(&mut out, name);
        }
        out.push_str("],\"uniqueConstraints\":[");
        for (j, unique) in table.unique_constraints.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str("{\"columns\":[");
            for (k, column) in unique.iter().enumerate() {
                if k > 0 {
                    out.push(',');
                }
                push_json_string(&mut out, column);
            }
            out.push_str("]}");
        }
        out.push_str("],\"indexes\":[");
        for (j, index) in table.indexes.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str("{\"name\":");
            push_json_string(&mut out, &index.name);
            out.push_str(",\"columns\":[");
            for (k, column) in index.columns.iter().enumerate() {
                if k > 0 {
                    out.push(',');
                }
                push_json_string(&mut out, column);
            }
            // Kept apart from `columns` on purpose: an entry here is
            // computed, not a column, and a differ that joined it against
            // `columns[].name` would be matching on a rendering.
            out.push_str("],\"expressions\":[");
            for (k, expression) in index.expressions.iter().enumerate() {
                if k > 0 {
                    out.push(',');
                }
                push_json_string(&mut out, expression);
            }
            out.push_str("],\"unique\":");
            out.push_str(if index.unique { "true" } else { "false" });
            out.push('}');
        }
        out.push_str("],\"foreignKeys\":[");
        for (j, fk) in table.foreign_keys.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str("{\"columns\":[");
            for (k, column) in fk.columns.iter().enumerate() {
                if k > 0 {
                    out.push(',');
                }
                push_json_string(&mut out, column);
            }
            out.push_str("],\"refTable\":");
            push_json_string(&mut out, &fk.ref_table);
            out.push_str(",\"refSchema\":");
            push_json_string(&mut out, &fk.ref_schema);
            out.push_str(",\"refColumns\":[");
            for (k, column) in fk.ref_columns.iter().enumerate() {
                if k > 0 {
                    out.push(',');
                }
                push_json_string(&mut out, column);
            }
            out.push_str("]}");
        }
        // Last in the object on purpose: the engine's own CREATE TABLE text
        // runs long, and the fields a reader scans for stay up front.
        out.push_str("],\"ddl\":");
        match &table.ddl {
            Some(sql) => push_json_string(&mut out, sql),
            None => out.push_str("null"),
        }
        out.push('}');
    }
    out.push_str("],\"sequences\":[");
    for (i, row) in sequence_rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"name\":");
        push_json_string(&mut out, &cell_str(row, 0));
        out.push_str(",\"start\":");
        // The executor already applied harbor's integer policy — bare within
        // JSON's exact range, quoted past it — so the value re-emits as is.
        match row.get(1) {
            Some(value) => out.push_str(&value.to_string()),
            None => out.push_str("null"),
        }
        out.push('}');
    }
    out.push_str("]}");

    let _ = req.respond(json_response(200, &out));
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

    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), Refusal>>(1);
    let (body_tx, body_rx) = mpsc::sync_channel::<Vec<u8>>(BODY_QUEUE);
    let job = Job {
        sql: "SELECT 1".to_string(),
        params: Vec::new(),
        shape: Shape::Ndjson,
        id: next_job_id(),
        // No deadline. A readiness probe that can time out would report the
        // database unready because harbor cancelled the probe, which is a
        // self-inflicted outage rather than a measurement.
        deadline: None,
        reset: false,
        ready: ready_tx,
        body: body_tx,
    };

    // The executor being gone is the failure this endpoint exists to catch, and
    // the one condition the worker must act on rather than merely report: it
    // returns `false` so the accept loop is left, exactly as `run_sql` does.
    if jobs.send(job).is_err() {
        *LAST_READY.lock().unwrap() = Some((Instant::now(), false));
        let _ = req.respond(error_response(503, "unready", "harbor is shutting down"));
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
            let _ = req.respond(error_response(503, "unready", &refusal.message));
            (true, 503)
        }
        Err(_) => {
            *LAST_READY.lock().unwrap() = Some((Instant::now(), false));
            let _ = req.respond(error_response(503, "unready", "the executor thread is gone"));
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
        let _ = req.respond(error_response(503, "unready", message));
        503
    }
}

/// Returns (keep serving, status sent). The first is false when the executor is
/// gone; see `handle`, which also writes the log line from the second.
fn run_sql(
    req: Request,
    jobs: &mpsc::SyncSender<Job>,
    state: &Arc<SlotState>,
    body: &str,
) -> (bool, u16) {
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

    if let Some(setting) = fenced_setting(&parsed.sql) {
        let _ = req.respond(error_response(
            400,
            "sql_error",
            &format!(
                "{setting} is fixed when the berth starts (harbor serve --memory-limit/--threads) \
                 and cannot be changed over the wire: it is process-global, and this berth may \
                 share its host with others (PLAN.md D2)"
            ),
        ));
        return (true, 400);
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
                Some(Claim { id: id.to_string(), sql: parsed.sql.clone(), target, state })
            }
            Err(refusal) => {
                let _ = req.respond(error_response(refusal.status, refusal.code, &refusal.message));
                return (true, refusal.status);
            }
        },
    };
    let target = claim.as_ref().map_or(jobs, |c| &c.target);
    // A lease statement runs on the lease's connection; everything else runs on
    // this worker's own. Cancellation has to name the one that will actually be
    // executing, not the one that accepted the request.
    let slot = claim.as_ref().map_or(state, |c| &c.state);

    let id = next_job_id();
    let deadline = parsed.timeout.map(|t| Instant::now() + t);

    // Registered before the job is sent, so a Stop pressed the instant the
    // query goes out has something to find. `Cancellable` deregisters on drop,
    // on every path below including the early returns.
    let _cancellable = match parsed.query.as_deref() {
        None => None,
        Some(name) => match Cancellable::register(name, slot, id) {
            Ok(guard) => Some(guard),
            Err(refusal) => {
                let _ = req.respond(error_response(refusal.status, refusal.code, &refusal.message));
                return (true, refusal.status);
            }
        },
    };

    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), Refusal>>(1);
    let (body_tx, body_rx) = mpsc::sync_channel::<Vec<u8>>(BODY_QUEUE);
    let job = Job {
        sql: parsed.sql,
        params: parsed.params,
        shape,
        id,
        deadline,
        reset: false,
        ready: ready_tx,
        body: body_tx,
    };

    if target.send(job).is_err() {
        // A lease whose executor is gone can never serve another statement, so
        // it is not merely a failed request — the lease itself is finished.
        // The worker keeps serving; only the lease dies.
        if let Some(c) = &claim {
            let id = c.id.clone();
            drop(claim);
            lease_release(&id);
            let _ = req.respond(error_response(503, "unavailable", "this session is gone"));
            return (true, 503);
        }
        let _ = req.respond(error_response(503, "unavailable", "harbor is shutting down"));
        return (false, 503);
    }

    match ready_rx.recv() {
        Ok(Ok(())) => {
            if shape == Shape::Json {
                // One message, because that is what the executor sends in this
                // shape — but drain the channel rather than assume it, so a
                // future change to how the document is chunked cannot silently
                // truncate a response.
                let mut document = Vec::new();
                while let Ok(chunk) = body_rx.recv() {
                    document.extend_from_slice(&chunk);
                }
                let headers = vec![
                    Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap(),
                    Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]).unwrap(),
                ];
                let length = document.len();
                let _ = req.respond(Response::new(
                    200.into(),
                    headers,
                    std::io::Cursor::new(document),
                    Some(length),
                ));
                return (true, 200);
            }
            // data_length: None makes justhttp chunk the body and keep the
            // connection alive.
            let headers = vec![
                Header::from_bytes(&b"Content-Type"[..], &b"application/x-ndjson"[..]).unwrap(),
                Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]).unwrap(),
            ];
            let _ = req.respond(Response::new(200.into(), headers, ChannelReader::new(body_rx), None));
            (true, 200)
        }
        Ok(Err(refusal)) => {
            let _ = req.respond(error_response(refusal.status, refusal.code, &refusal.message));
            (true, refusal.status)
        }
        Err(_) => {
            let _ = req.respond(error_response(500, "internal", "the executor thread is gone"));
            (false, 500)
        }
    }
}

/// Advance `i` past ASCII whitespace and SQL comments — `--` to end of line,
/// nested `/* */`. The one comment/whitespace skipper the token scanners
/// share (first_keyword, fenced_setting). `ensure_single_statement` keeps its
/// own inline scan: it is a full-byte security lexer, not a tokenizer, and is
/// pinned by its own tests.
fn skip_trivia(b: &[u8], i: &mut usize) {
    loop {
        while *i < b.len() && b[*i].is_ascii_whitespace() {
            *i += 1;
        }
        if b[*i..].starts_with(b"--") {
            // CR ends the comment too — see ensure_single_statement. The same
            // one-byte gap defeated the fleet-safety fence from the other
            // side: `SET --\r memory_limit='1TB'` looked like a bare `SET`
            // with a trailing comment here, so `fenced_setting` never saw the
            // key, while the engine set it. memory_limit is process-global,
            // so that is every neighbor berth's ceiling raised by one caller
            // (PLAN.md D2) — measured going from 1.8 GiB to 931.3 GiB.
            *i = b[*i..]
                .iter()
                .position(|&c| c == b'\n' || c == b'\r')
                .map_or(b.len(), |p| *i + p + 1);
        } else if b[*i..].starts_with(b"/*") {
            let mut depth = 1;
            *i += 2;
            while *i < b.len() && depth > 0 {
                if b[*i..].starts_with(b"/*") {
                    depth += 1;
                    *i += 2;
                } else if b[*i..].starts_with(b"*/") {
                    depth -= 1;
                    *i += 2;
                } else {
                    *i += 1;
                }
            }
        } else {
            break;
        }
    }
}

/// The next token after trivia, uppercased: an identifier run
/// (`[A-Za-z0-9_]`) or a `"double-quoted"` identifier (`""` escapes an inner
/// quote). Quoted and bareword forms name the same thing to DuckDB, so the
/// fence must read both. Empty at end of input; on stray punctuation it
/// consumes one byte and returns it, so a caller loop always makes progress.
fn next_word(b: &[u8], i: &mut usize) -> String {
    skip_trivia(b, i);
    if *i < b.len() && b[*i] == b'"' {
        *i += 1;
        let mut ident = String::new();
        while *i < b.len() {
            if b[*i] == b'"' {
                if b.get(*i + 1) == Some(&b'"') {
                    ident.push('"');
                    *i += 2;
                    continue;
                }
                *i += 1;
                break;
            }
            ident.push(b[*i] as char);
            *i += 1;
        }
        return ident.to_ascii_uppercase();
    }
    let start = *i;
    while *i < b.len() && (b[*i].is_ascii_alphanumeric() || b[*i] == b'_') {
        *i += 1;
    }
    if *i == start && *i < b.len() {
        *i += 1;
    }
    std::str::from_utf8(&b[start..*i]).unwrap_or("").to_ascii_uppercase()
}

/// The first real word of a statement, upper-cased, skipping leading
/// whitespace and comments. Empty when there is no leading identifier — a
/// statement starting with `(`, or nothing at all.
fn first_keyword(sql: &str) -> String {
    next_word(sql.as_bytes(), &mut 0)
}

/// Settings a client must not change: they are process-global in DuckDB, so
/// one `SET memory_limit='100GB'` raises it for every neighbor berth on the
/// host and defeats the fleet-safe cap the operator chose at berth start
/// (PLAN.md D2). Verified live: the SET took effect for all workers at once.
///
/// Reads through comments, whitespace, and double-quoting via `next_word`, so
/// neither `/*x*/ SET threads=8` nor `SET "memory_limit"=…` slips past.
fn fenced_setting(sql: &str) -> Option<&'static str> {
    const FENCED: &[&str] = &[
        "memory_limit",
        "max_memory",
        "threads",
        "worker_threads",
        "external_threads",
        // Disk spill is process-global too, and the operator caps it with
        // `--max-temp-size` precisely so one query cannot fill the shared host
        // disk (PLAN.md D2). Left unfenced, `SET max_temp_directory_size='100TB'`
        // over the wire erases that cap; `temp_directory` redirects the spill
        // itself. Both are GLOBAL-scope in DuckDB — same class as the rest here.
        "max_temp_directory_size",
        "temp_directory",
    ];
    let b = sql.as_bytes();
    let mut i = 0;
    if !matches!(next_word(b, &mut i).as_str(), "SET" | "RESET" | "PRAGMA") {
        return None;
    }
    let mut name = next_word(b, &mut i);
    if matches!(name.as_str(), "GLOBAL" | "SESSION" | "LOCAL") {
        name = next_word(b, &mut i);
    }
    FENCED.iter().find(|f| name.eq_ignore_ascii_case(f)).copied()
}

/// What a statement does to the surrounding transaction, when that is knowable
/// from its first word: `Some(true)` opens one, `Some(false)` ends one, `None`
/// leaves it as it was. Used to report whether a lease is holding a
/// transaction open, which is the thing an operator most needs to see.
fn transaction_effect(sql: &str) -> Option<bool> {
    match first_keyword(sql).as_str() {
        "BEGIN" | "START" => Some(true),
        "COMMIT" | "END" | "ROLLBACK" | "ABORT" => Some(false),
        _ => None,
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
    let word = first_keyword(sql);
    if word.is_empty() {
        return true;
    }
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
/// `ROLLBACK` before the next job. Split out of `execute_jobs` so the whole
/// thing runs under one `catch_unwind` there: a panic in the DuckDB client (a
/// decoder that hits `unreachable!`, a metadata assert) must not take the
/// executor thread — and with it a worker and a pool slot — down for good.
// Eight, and deliberately. This exists to be the whole of what runs under
// one catch_unwind in execute_jobs, so every value that unwind must not
// straddle is passed in rather than captured. Bundling them into a struct
// would hide exactly the thing the split was made to show.
#[allow(clippy::too_many_arguments)]
fn run_statement(
    conn: &Connection,
    on_slot: &mut OnSlot,
    sql: String,
    params: Vec<Value>,
    shape: Shape,
    ready: mpsc::SyncSender<Result<(), Refusal>>,
    body: mpsc::SyncSender<Vec<u8>>,
    started: Instant,
) -> bool {
    // Decided from the statement text before it runs, then widened below by
    // any path that ends the job early.
    let mut needs_reset = may_leave_transaction_open(&sql);

    // Cached by SQL text (per-connection LRU in duckdb-rs), so a repeated
    // statement skips DuckDB's parse+plan — the dominant engine cost for
    // small SQL, and the mitigation for v2's slower parser. Behavior across
    // catalog changes is gated empirically by test/sql (drop/recreate a
    // referenced table, then re-run the identical text).
    let stmt = match conn.prepare_cached(&sql) {
        Ok(s) => s,
        Err(e) => {
            let _ = ready.send(Err(refusal_for(on_slot.finish(), e.to_string())));
            return true;
        }
    };
    let mut stmt = stmt;
    let mut rows = match stmt.query(params_from_iter(params.iter())) {
        Ok(r) => r,
        Err(e) => {
            let _ = ready.send(Err(refusal_for(on_slot.finish(), e.to_string())));
            return true;
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
        let _ = ready.send(Err(Refusal::sql(format!(
            "harbor cannot encode {} columns: the DuckDB Rust client has no \
             decoder for this type. Cast it — {}::VARCHAR, or {}::TIME for \
             microsecond precision — and the value comes back intact.",
            type_name(bad),
            name,
            name
        ))));
        return true;
    }

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
    // large-path allocation per statement. The post-flush refill below keeps
    // the full capacity, so a streaming result allocates big exactly once.
    let mut buf = String::with_capacity(4096);
    match shape {
        Shape::Ndjson => buf.push_str(r#"{"type":"schema","columns":["#),
        // v1's envelope also carried `kind`, "select" or "write". It is
        // not emitted here, because there is no definition of it that is
        // right: DuckDB answers CREATE TABLE with a one-column `Count`
        // result, so "did the statement produce columns" calls a write a
        // select, and deciding from the leading keyword is a parser that
        // exists only to label something no client needs — `columns` and
        // `rowCount` already say everything it could. A field that is
        // absent is easier to handle than one that lies.
        Shape::Json => buf.push_str(r#"{"ok":true,"columns":["#),
    }
    for (i, (name, ty)) in names.iter().zip(&types).enumerate() {
        if i > 0 {
            buf.push(',');
        }
        emit_column_schema(&mut buf, Some(name), ty);
    }
    match shape {
        Shape::Ndjson => buf.push_str("]}\n"),
        Shape::Json => buf.push_str(r#"],"data":["#),
    }

    let mut count: u64 = 0;
    let mut gone = false;
    // Set when the result cannot be completed. In NDJSON it has already
    // been written into the stream by the time it is set; in one-shot it is
    // what the request fails with.
    let mut failure: Option<Refusal> = None;
    loop {
        match rows.next() {
            Ok(Some(row)) => {
                // The safety net behind the pre-flight check above. A
                // panic in here would otherwise kill this executor
                // thread, and the damage is worse than one failed query:
                // the client sees 200 with an empty body — success, no
                // rows — and the connection never returns to the pool, so
                // enough such queries take the server out of service.
                // Encoded straight into `buf` behind a mark: a panicking
                // decoder discards the half-written row with truncate()
                // instead of paying a scratch String + copy per row.
                let mark = buf.len();
                let encoded = std::panic::catch_unwind(AssertUnwindSafe(|| {
                match shape {
                    Shape::Ndjson => buf.push_str(r#"{"type":"row","values":["#),
                    Shape::Json => {
                        if count > 0 {
                            buf.push(',');
                        }
                        buf.push('[');
                    }
                }
                for (i, ty) in types.iter().enumerate() {
                    if i > 0 {
                        buf.push(',');
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
                        buf.push_str("null");
                        continue;
                    }
                    match row.get_ref(i) {
                        // A UNION's tag says which member is set, and
                        // `Value` drops it — union_value(a := 2) and
                        // union_value(b := 2) would be indistinguishable.
                        // The tag is still on the arrow array underneath.
                        Ok(v) => {
                            let tag = union_tag(&v);
                            emit_tagged(&mut buf, tag, &Value::from(v), Some(ty));
                        }
                        Err(_) => buf.push_str("null"),
                    }
                }
                match shape {
                    Shape::Ndjson => buf.push_str("]}\n"),
                    Shape::Json => buf.push(']'),
                }
                }));

                if encoded.is_err() {
                    buf.truncate(mark);
                    let message = "harbor cannot encode a value in this result: the DuckDB \
                         Rust client has no decoder for one of its column types. The query \
                         ran; the value cannot be represented. Cast the column to VARCHAR \
                         to see it.";
                    if shape == Shape::Ndjson {
                        // The headers are long gone, so this can only be
                        // said in the stream — but it is said, rather than
                        // the client being left to infer it from a short
                        // result.
                        buf.push_str(
                            r#"{"type":"error","code":"unsupported_type","message":"#,
                        );
                        push_json_string(&mut buf, message);
                        buf.push_str("}\n");
                        let _ = body.send(std::mem::take(&mut buf).into_bytes());
                        gone = true;
                    }
                    failure = Some(Refusal {
                        status: 400,
                        code: "unsupported_type",
                        message: message.to_string(),
                    });
                    break;
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
                                break;
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
                                code: "response_too_large",
                                message: format!(
                                    "this result is larger than the {} MiB harbor will hold \
                                     in memory for a single JSON document. Ask for NDJSON \
                                     instead — send no Accept header, or Accept: \
                                     application/x-ndjson — and it streams with no size \
                                     limit.",
                                    MAX_JSON_RESPONSE >> 20
                                ),
                            });
                            break;
                        }
                    }
                }
            }
            Ok(None) => break,
            Err(e) => {
                // Retired here rather than after the loop, because what the
                // client is told depends on the answer: the same DuckDB
                // error means "your SQL failed" or "you cancelled this",
                // and only the slot knows which.
                let refusal = refusal_for(on_slot.finish(), e.to_string());
                if shape == Shape::Ndjson {
                    // Mid-stream failures cannot change the status code —
                    // the headers are long gone. Say so in the stream, so a
                    // client never mistakes a truncated result for a
                    // complete one. The code travels from the refusal: it
                    // used to be the literal "sql_error", which labelled a
                    // cancellation and an unsupported type as bad SQL.
                    buf.push_str(r#"{"type":"error","code":""#);
                    buf.push_str(refusal.code);
                    buf.push_str(r#"","message":"#);
                    push_json_string(&mut buf, &refusal.message);
                    buf.push_str("}\n");
                    let _ = body.send(std::mem::take(&mut buf).into_bytes());
                    gone = true;
                }
                failure = Some(refusal);
                break;
            }
        }
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

/// The DuckDB side. Owns one connection for the life of the server and runs
/// one statement at a time; concurrency comes from there being several of
/// these, not from any one of them interleaving work. `pinned` marks a lease
/// connection: the per-job reset that stops one request's stray transaction
/// from leaking into the next request on the same connection must not fire on
/// a lease, because holding that transaction open is precisely what a lease is
/// for — the rollback happens instead when the lease is released, by commit,
/// by DELETE, or by the reaper.
fn execute_jobs(
    conn: Connection,
    jobs: mpsc::Receiver<Job>,
    pinned: bool,
    state: Arc<SlotState>,
) -> Connection {
    // Room for a working set of distinct statement texts (dashboards cycle
    // through dozens); duckdb-rs's default LRU of 16 thrashes too easily.
    conn.set_prepared_statement_cache_capacity(64);
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
        if needs_reset && !pinned {
            reset_transaction(&conn);
        }

        let Job { sql, params, shape, id, deadline, reset, ready, body } = job;
        if reset {
            // Same call the workers make between requests, and unconditional:
            // this runs once per lease release, not once per statement, so the
            // throughput argument that made it conditional there does not
            // apply here.
            reset_transaction(&conn);
            needs_reset = false;
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
            // has touched the connection, so there is nothing to roll back.
            on_slot.finish();
            let _ = ready.send(Err(Refusal::cancelled()));
            continue;
        }

        // A panic below — a duckdb-rs decoder that hits `unreachable!` on some
        // column type, a metadata call that trips an assert — used to unwind
        // straight out of this thread. The worker then found the job channel
        // closed on its next send, left the accept loop (a worker with no
        // executor answers 503 by return and would win every race), and the
        // slot was gone for the life of the process; a handful of such queries
        // retire every worker until the berth answers only 503. The per-row
        // guard inside `run_statement` catches the common value-decode panic;
        // catching the whole statement here covers one in prepare, metadata, or
        // schema too. On a panic the `OnSlot` guard drops — retiring the slot —
        // the waiting worker is told (500), and this executor takes the next
        // job. The connection itself is intact (the panic was in Rust-side
        // encoding, not DuckDB's engine), so the next job resets first.
        let ready_guard = ready.clone();
        needs_reset = match std::panic::catch_unwind(AssertUnwindSafe(|| {
            run_statement(&conn, &mut on_slot, sql, params, shape, ready, body, started)
        })) {
            Ok(next_reset) => next_reset,
            Err(_) => {
                let _ = ready_guard.send(Err(Refusal {
                    status: 500,
                    code: "internal",
                    message: "harbor recovered from an internal error while \
                              handling this statement"
                        .to_string(),
                }));
                true
            }
        };
    }
    // And once more on the way out, so a connection going back to the pool for
    // the next harbor_serve is clean too. Unconditional here: this runs once
    // per server lifetime, so the extra statement costs nothing.
    reset_transaction(&conn);
    conn
}

/// Adapts the body channel to the `Read` justhttp wants. Returning `Ok(0)`
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
    s.push_str(r#"{"type":"error","code":"#);
    push_json_string(&mut s, code);
    s.push_str(r#","message":"#);
    push_json_string(&mut s, message);
    s.push('}');
    json_response(status, &s)
}

// ---------------------------------------------------------------------------

/// Bytes of entropy, for a token nobody has to invent. Not a hot path, so the
/// cost of `getrandom` per call is irrelevant.
pub fn random_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}


#[cfg(test)]
mod tests {
    use super::Method;
    use crate::encode::civil_from_days;
    use super::ensure_single_statement as one;
    use super::fenced_setting;
    use super::route_exists;
    use super::index_columns;
    use super::{IndexPart, index_parts};
    use crate::encode::varint_to_decimal;
    use super::{Cancel, SlotRun};
    use std::time::{Duration, Instant};

    fn idle() -> SlotRun {
        SlotRun { job: 0, started: Instant::now(), pending: None, cancelled: false, deadline: None, request: None }
    }

    /// The bug this design exists to prevent: a cancel decided for one
    /// statement must not fire on the next one to run on that connection.
    /// Without the id, "is something running?" is true in both cases and the
    /// interrupt lands on an innocent query.
    #[test]
    fn a_cancel_never_lands_on_the_next_statement() {
        let mut run = idle();
        run.begin(7, None);
        // Job 7 finishes before the cancel is decided.
        assert!(!run.end());
        // The next statement starts on the same connection.
        run.begin(8, None);
        // A cancel aimed at 7 arrives now. It must not touch 8.
        assert_eq!(run.arm(Some(7)), Cancel::Held);
        assert!(!run.cancelled, "job 8 was marked cancelled by a cancel aimed at job 7");
        assert!(!run.end(), "job 8 reported itself cancelled");
    }

    /// And the held cancel must not survive to ambush a later statement
    /// either — it named an id that will never run again.
    #[test]
    fn a_held_cancel_is_discarded_by_the_next_statement() {
        let mut run = idle();
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
        let mut run = idle();
        assert_eq!(run.arm(Some(4)), Cancel::Held);
        assert!(run.begin(4, None), "job 4 ran despite being cancelled before it started");
    }

    #[test]
    fn cancelling_an_idle_slot_does_nothing() {
        let mut run = idle();
        assert_eq!(run.arm(None), Cancel::Nothing);
        assert!(!run.cancelled);
    }

    #[test]
    fn cancelling_whatever_is_running_does_not_need_an_id() {
        let mut run = idle();
        run.begin(3, None);
        assert_eq!(run.arm(None), Cancel::Fire);
        assert!(run.end(), "the statement did not report itself cancelled");
    }

    /// `end` clears the flag as well as reading it, so the next statement on
    /// this connection starts clean. A latched flag would report every
    /// subsequent query on that worker as cancelled.
    #[test]
    fn the_cancelled_flag_does_not_outlive_its_statement() {
        let mut run = idle();
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
        let mut run = idle();
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

    /// DuckDB allows `$` inside an identifier, so `a$b$c` is one identifier —
    /// not a dollar-quoted string. Reading it as an opener made the scanner
    /// hunt for a close that never came and swallow the rest of the input,
    /// terminator and all, so the second statement ran during `prepare`.
    /// Found by differential fuzzing against the engine, which is also the
    /// only way to be confident about the cases still not listed here.
    #[test]
    fn rejects_a_statement_after_a_dollar_inside_an_identifier() {
        for sql in [
            "SELECT 1 a$b$c; DROP TABLE orders",
            "SELECT a$b$c; DROP TABLE orders",
            "SELECT 1 a$b$c$$; DROP TABLE orders",
            "SELECT 1 a$b$c\r; DROP TABLE orders",
            "SELECT 1 x$1$; DROP TABLE orders",
        ] {
            assert!(one(sql).is_err(), "should reject: {sql:?}");
        }
        // ...while a real dollar-quote, opened where one can be opened, still
        // hides its terminator exactly as before.
        for sql in ["SELECT a$b$c", "SELECT $$a; b$$", "SELECT $t$a; b$t$", "SELECT 'x'$$a;b$$"] {
            assert!(one(sql).is_ok(), "should accept: {sql:?}");
        }
    }

    /// A bare CR ends a `--` comment for DuckDB (Postgres lexer heritage), so
    /// everything after one is a second statement. Scanning for LF alone made
    /// each of these look like a single statement with a trailing comment,
    /// and `duckdb-rs` executes all but the last during `prepare` — so the
    /// DROP ran before a row was fetched. Verified live against the engine
    /// before the fix: the table was gone and the response carried the DROP's
    /// own `Success BOOLEAN` schema.
    #[test]
    fn rejects_a_statement_hidden_behind_a_cr_terminated_comment() {
        for sql in [
            "SELECT 1 --\r; DROP TABLE orders",
            "SELECT 1 -- note\r; DROP TABLE orders",
            "SELECT 1 --\r\n; DROP TABLE orders",
            "SET --\r memory_limit='1TB'; SELECT 1",
        ] {
            assert!(one(sql).is_err(), "should reject: {sql:?}");
        }
    }

    /// The other side of the same byte: CR must not end a comment that is only
    /// data, and a comment that really does run to the end of the input is
    /// still a single statement.
    #[test]
    fn a_cr_inside_a_literal_is_not_a_comment_terminator() {
        for sql in [
            "SELECT 1 -- trailing\r",
            "SELECT 1 -- trailing\r\n",
            "SELECT '--\r; DROP TABLE orders'",
            "SELECT 1 /* \r; still one comment */",
        ] {
            assert!(one(sql).is_ok(), "should accept: {sql:?}");
        }
    }

    /// The fleet-safety fence (D2) reads through comments with the same
    /// scanner, and fell to the same byte from the other direction: the key
    /// hid behind a CR-terminated comment, so `fenced_setting` saw a bare
    /// `SET` and passed it, while the engine set a process-global limit.
    #[test]
    fn the_fence_sees_a_key_behind_a_cr_terminated_comment() {
        for sql in [
            "SET --\r memory_limit='1TB'",
            "SET --x\r memory_limit='1TB'",
            "PRAGMA --\r threads=64",
            "RESET --\r\n threads",
        ] {
            assert!(fenced_setting(sql).is_some(), "should be fenced: {sql:?}");
        }
        // ...and an unrelated key behind the same comment still passes.
        assert_eq!(fenced_setting("SET --\r timezone='UTC'"), None);
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

    /// `indexes[].columns` exists to be joined against `columns[].name`, so
    /// an identifier that needed quoting has to arrive unquoted — three of
    /// five names on an ordinary table failed to match before this. Anything
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

    #[test]
    fn fences_process_global_settings() {
        // The fleet-safety fence (D2): these change a DuckDB global and must
        // not be settable over the wire.
        for sql in [
            "SET memory_limit='1TB'",
            "set MEMORY_LIMIT = '1TB'",
            "PRAGMA threads=64",
            "SET GLOBAL max_memory='1TB'",
            "RESET threads",
            "/* sneak */ SET memory_limit='1TB'",
            "SET  --c\n threads=1",
            // the quoted-identifier bypass: a double-quoted name reaches the
            // same setting, so the fence must see through the quotes
            "SET \"memory_limit\"='1TB'",
            "PRAGMA \"threads\"=64",
            "SET GLOBAL \"max_memory\"='1TB'",
            "SET \"me\"\"mory_limit\"='1TB'", // (not a real key, but scanner must unquote)
            // disk-spill caps are fenced too — SET around --max-temp-size
            "SET max_temp_directory_size='100TB'",
            "SET temp_directory='/tmp/x'",
            "PRAGMA \"max_temp_directory_size\"='100TB'",
        ] {
            let got = fenced_setting(sql);
            let expect_fenced = !sql.contains("me\"\"mory");
            assert_eq!(got.is_some(), expect_fenced, "fence verdict for {sql:?}");
        }
        // Ordinary statements and unrelated settings pass.
        for sql in [
            "SELECT 1",
            "SET timezone='UTC'",
            "SET \"search_path\"='main'",
            "CREATE TABLE t(x int)",
            "PRAGMA database_list",
        ] {
            assert_eq!(fenced_setting(sql), None, "should pass: {sql:?}");
        }
    }

    #[test]
    fn route_exists_matches_the_dispatch_table() {
        // Guards the hand-maintained coupling between `route_exists` (which
        // decides 401-vs-404 for an unauthenticated caller) and the dispatch
        // match in `handle`. Every real endpoint is a route; a known path
        // with the wrong method, and any unknown path, is not — so adding a
        // route to `handle` without updating `route_exists` (which would
        // 404 a real endpoint's unauthenticated caller) fails here.
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
        let ids = [wire::endpoint::session("abc"), wire::endpoint::query("xyz")];
        for r in wire::endpoint::FIXED.iter().chain(ids.iter()) {
            assert!(route_exists(&method(r.method), &r.path), "wire publishes {r}, harbor does not serve it");
        }
        let non_routes = [
            (Method::Get, "/sql"),      // method matters
            (Method::Post, "/ready"),   // method matters
            (Method::Get, "/health"),   // never existed
            (Method::Get, "/"),
            (Method::Put, "/sql"),
            (Method::Post, "/sql/sessions"), // no trailing id/segment
        ];
        for (m, p) in &non_routes {
            assert!(!route_exists(m, p), "should NOT be a route: {m:?} {p}");
        }
    }
}
