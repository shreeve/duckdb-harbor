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
        Arc, Condvar, Mutex, OnceLock, PoisonError, RwLock,
        mpsc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use justhttp::{Header, Method, Request, Response, Server};
use wire::catalog::{
    Attached, AttachedInventory, Catalog, Column, ForeignKey, Index, Inventory, Named, Sequence, Table, Unique, View,
};
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

// The workspace database: what a server that opens it does at its start and
// at its stop.
pub mod workspace;

// The catalog's views, bound off the pool and within a budget.
mod binder;

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
    /// The statement is in the engine, which runs it to its end before it
    /// hands back a chunk (`Conn::execute`'s retained statement), and its
    /// first fetch has not come back. An interrupt that reaches it in this
    /// stretch may stop it or may come too late, with the statement done
    /// and, in autocommit, committed, and only that fetch says which. A
    /// canceller who fires in it waits for the fetch, so a cancel is
    /// answered true only when the statement was stopped and nothing of it
    /// was kept.
    retained: bool,
    /// A retained statement's first fetch came back: what it did is done,
    /// and a cancel from here stops nothing.
    kept: bool,
    /// Where a canceller waiting on a retained statement is told whether it
    /// stopped: true when it ended cancelled, false when it was kept.
    told: Option<Arc<OnceLock<bool>>>,
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
        SlotRun {
            job: 0,
            last: 0,
            started: Instant::now(),
            pending: None,
            cancelled: false,
            retained: false,
            kept: false,
            told: None,
            deadline: None,
            wedged_at: None,
        }
    }

    /// Claim this slot for `job`. Returns true when the statement was already
    /// cancelled before it began, in which case it must not run at all.
    fn begin(&mut self, job: u64, deadline: Option<Instant>) -> bool {
        self.job = job;
        self.last = job;
        self.started = Instant::now();
        self.deadline = deadline;
        self.retained = false;
        self.kept = false;
        // Any held cancel is consumed here whether or not it matches: it named
        // a statement that is now either this one or one that will never start,
        // and either way it has had its say.
        self.cancelled = self.pending.take() == Some(job);
        self.cancelled
    }

    fn end(&mut self) -> bool {
        self.job = 0;
        self.deadline = None;
        self.retained = false;
        self.kept = false;
        let cancelled = std::mem::replace(&mut self.cancelled, false);
        // A retained statement that ends before its first fetch came back
        // ends cancelled, as a 499, only when a canceller reached it.
        self.tell(cancelled);
        cancelled
    }

    /// The statement is submitted and about to run. False when a canceller
    /// has reached it already, and it must not run.
    fn submitted(&mut self, retained: bool) -> bool {
        if self.cancelled {
            return false;
        }
        self.retained = retained && self.job != 0;
        true
    }

    /// A retained statement's first fetch came back: it ran to its end. A
    /// cancel that fired at it came too late and stopped nothing, so it is
    /// cleared, and the statement is answered as what it did.
    fn keep(&mut self) {
        if !self.retained {
            return;
        }
        self.retained = false;
        self.kept = true;
        self.cancelled = false;
        self.tell(false);
    }

    fn tell(&mut self, stopped: bool) {
        if let Some(told) = self.told.take() {
            let _ = told.set(stopped);
        }
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
            _ if self.job == 0 || self.kept => Cancel::Nothing,
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
        let mut run = self.run.lock().unwrap();
        let asked = run.told.is_some();
        let cancelled = run.end();
        drop(run);
        if asked {
            Asked::said();
        }
        cancelled
    }

    /// Mark the statement submitted, unless a canceller has reached it; see
    /// `SlotRun::submitted`.
    fn submitted(&self, retained: bool) -> bool {
        self.run.lock().unwrap().submitted(retained)
    }

    /// A retained statement's first fetch came back; see `SlotRun::keep`.
    fn keep(&self) {
        let mut run = self.run.lock().unwrap();
        let asked = run.told.is_some();
        run.keep();
        drop(run);
        if asked {
            Asked::said();
        }
    }

    /// Whether a canceller has reached the statement now running.
    fn cancelled(&self) -> bool {
        self.run.lock().unwrap().cancelled
    }

    /// Interrupt the running statement if it is still `job` — or whatever is
    /// running, when `job` is None. Returns whether it fired, or held the
    /// cancel for a statement yet to begin: what the reapers, deadlines and
    /// departures that call it need, none of which waits on the answer.
    fn cancel(&self, job: Option<u64>) -> bool {
        let mut run = self.run.lock().unwrap();
        self.fire(&mut run, job)
    }

    /// Cancel `job` and say whether that stopped it, which is what a client
    /// that asked is told. Fired at a statement harbor stops itself, the
    /// answer is known at once: one that has not begun never runs, and one
    /// that has is answered 499 at its next step or at its end. Fired at a
    /// retained statement in the engine, it is the engine's, and is told at
    /// the statement's first fetch: an INSERT the interrupt did not reach in
    /// time has run, in autocommit committed, and the cancel stopped
    /// nothing. That is as long as the engine takes to notice the interrupt
    /// or to finish, and the statement's executor waits on nothing but the
    /// engine meanwhile.
    fn cancel_and_tell(&self, job: u64) -> Said {
        let mut run = self.run.lock().unwrap();
        if !self.fire(&mut run, Some(job)) {
            return Said::Now(false);
        }
        if !(run.job == job && run.retained) {
            return Said::Now(true);
        }
        Said::Later(Arc::clone(run.told.get_or_insert_with(Default::default)))
    }

    /// `cancel_and_tell`, waited for on this thread.
    #[cfg(test)]
    fn cancel_and_say(&self, job: u64) -> bool {
        match self.cancel_and_tell(job) {
            Said::Now(stopped) => stopped,
            Said::Later(told) => Asked::wait(&told),
        }
    }

    fn fire(&self, run: &mut SlotRun, job: Option<u64>) -> bool {
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

    /// The running job's id if a canceller has reached it, else None.
    fn unheeded_job(&self) -> Option<u64> {
        let run = self.run.lock().unwrap();
        (run.job != 0 && run.cancelled).then_some(run.job)
    }

    /// The job running right now (0 = idle). A snapshot for reapers that must
    /// name their target instead of firing at "whatever is running".
    fn current_job(&self) -> u64 {
        self.run.lock().unwrap().job
    }
}

/// Whether a cancel stopped its statement: known when it fired, or told at
/// a retained statement's first fetch (`SlotRun::told`).
enum Said {
    Now(bool),
    Later(Arc<OnceLock<bool>>),
}

/// A `DELETE /sql/queries/<id>` waiting to be told whether its cancel
/// stopped a retained statement, or one the probe lane answers. A few
/// threads answer them all (`answer_cancels`), so a wait holds neither a
/// worker nor the probe lane, which answers `/ready`, renewals, releases and
/// other cancels: the wait is the engine's longest stretch without an
/// interrupt check, a commit that checkpoints or a `CHECKPOINT`, and the
/// lane must not stand behind it, nor behind a client that does not read.
struct Asked {
    told: Arc<OnceLock<bool>>,
    req: Request,
    line: Option<LogLine>,
}

/// The cancels waiting to be told and answered, and which server's threads
/// answer them.
struct Asking {
    waiting: Vec<Asked>,
    /// The server whose `answer_cancels` threads answer, numbered from one,
    /// or zero when none does.
    serving: u64,
}

/// The cancels waiting, and the signal a statement gives when it tells one:
/// at its first fetch (`SlotState::keep`) or its end.
static ASKED: (Mutex<Asking>, Condvar) = (Mutex::new(Asking { waiting: Vec::new(), serving: 0 }), Condvar::new());

/// How many threads answer the cancels told. Each answer is written whole by
/// one of them, which a client that does not read its answer holds for as
/// long as its socket's write timeout, while the others go on answering.
const CANCEL_WRITERS: usize = 4;

impl Asked {
    /// A statement told its canceller. Signalled under the lock the waiters
    /// check under, so none checks before the telling and sleeps after it.
    fn said() {
        let _asked = ASKED.0.lock().unwrap();
        ASKED.1.notify_all();
    }

    /// Wait on this thread until `told` is.
    fn wait(told: &OnceLock<bool>) -> bool {
        let (asked, said) = &ASKED;
        let _asked = said.wait_while(asked.lock().unwrap(), |_| told.get().is_none()).unwrap();
        *told.get().unwrap()
    }
}

/// Answer the cancels in ASKED as their statements tell them, one at a
/// time, while `serving` is the server's; then the ones told, and end. A
/// statement harbor abandoned at a stop never tells, and its cancel is left
/// unanswered, as its statement is. A cancel is handed out only once the
/// response before it on its connection is written, so its answer waits on
/// no statement.
fn answer_cancels(serving: u64) {
    let (asked, said) = &ASKED;
    loop {
        let mut asking = said
            .wait_while(asked.lock().unwrap(), |a| {
                a.serving == serving && !a.waiting.iter().any(|a| a.told.get().is_some())
            })
            .unwrap();
        let Some(at) = asking.waiting.iter().position(|a| a.told.get().is_some()) else { return };
        let Asked { told, req, line } = asking.waiting.swap_remove(at);
        drop(asking);
        let status = answer_cancel(req, *told.get().unwrap()).1;
        if let Some(line) = line {
            line.write(status);
        }
    }
}

/// Start the threads that answer cancels for a server, numbered `serving`.
fn spawn_answering(serving: u64) -> std::io::Result<Vec<JoinHandle<()>>> {
    ASKED.0.lock().unwrap().serving = serving;
    (0..CANCEL_WRITERS)
        .map(|i| thread::Builder::new().name(format!("harbor-cancel-{i}")).spawn(move || answer_cancels(serving)))
        .collect()
}

/// Let the threads that answer cancels end once each told is answered, and
/// wait up to `patience` for them: one writing to a client that does not
/// read is let go of, as a worker is.
fn end_answering(answering: Vec<JoinHandle<()>>, patience: Duration) {
    ASKED.0.lock().unwrap().serving = 0;
    ASKED.1.notify_all();
    let started = Instant::now();
    for h in answering {
        while !h.is_finished() && started.elapsed() < patience {
            thread::sleep(Duration::from_millis(10));
        }
        if h.is_finished() {
            let _ = h.join();
        }
    }
}

/// Answer `req`, a cancel told or to be told by `told`, from
/// `answer_cancels`, or on this thread when no server's threads answer.
fn await_cancel(req: Request, told: Arc<OnceLock<bool>>, line: Option<LogLine>) -> bool {
    let mut asking = ASKED.0.lock().unwrap();
    if asking.serving != 0 {
        asking.waiting.push(Asked { told, req, line });
        drop(asking);
        ASKED.1.notify_all();
        return true;
    }
    drop(asking);
    let status = answer_cancel(req, Asked::wait(&told)).1;
    if let Some(line) = line {
        line.write(status);
    }
    true
}

fn answer_cancel(req: Request, cancelled: bool) -> (bool, u16) {
    let body = serde_json::to_string(&wire::CancelledResponse { cancelled }).unwrap();
    let _ = req.respond(json_response(200, &body));
    (true, 200)
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
fn cancel_query(id: &str) -> Said {
    let target = {
        let guard = QUERIES.lock().unwrap();
        guard.as_ref().and_then(|q| q.get(id).map(|(s, j)| (Arc::clone(s), *j)))
    };
    match target {
        Some((slot, job)) => slot.cancel_and_tell(job),
        None => Said::Now(false),
    }
}

/// Stop any statement that has outlived its deadline, and interrupt again
/// any a canceller has reached that is still running. Runs on the reaper's
/// tick, so the granularity of a timeout is the reap interval — which is the
/// right trade for a limit measured in seconds and enforced by a thread that
/// would otherwise be asleep.
///
/// The engine can lose an interrupt: one that lands a moment after a
/// statement is submitted was measured to leave it running as if nobody had
/// asked, a scan over a document param to its deadline, with its cancel
/// answered true. A retained statement's canceller would wait on it all
/// that time. So a cancel is fired again each tick for as long as its
/// statement runs, which is harmless: it is aimed by id, and the engine
/// clears a stray one as the next statement starts.
fn cancel_expired() {
    let slots: Vec<Arc<SlotState>> = SLOTS.lock().unwrap().clone();
    let now = Instant::now();
    for slot in slots {
        // By id, never "whatever is running": between noticing the expiry and
        // firing, the expired statement can finish and a fresh one begin, and
        // a cancel(None) would kill that innocent — the exact race the job-id
        // machinery exists to close (see SlotRun). A stale id finds nothing.
        if let Some(job) = slot.expired_job(now).or_else(|| slot.unheeded_job()) {
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
    /// Learns the names again while they are not all known
    /// (`learn_while_unknown`).
    learner: Option<JoinHandle<()>>,
    /// The saturation-proof lane: answers /ready and the control plane when
    /// every worker is busy, and relays sessions' statements. See probe_worker.
    probe: Option<JoinHandle<()>>,
    /// The threads that answer cancels (`answer_cancels`).
    answering: Vec<JoinHandle<()>>,
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
    // What --init and the workspace attached are in place, and attaching in
    // the background may still be under way. A SQLite file locked at the
    // start leaves the names to a later chance, not the start to its wait.
    *LEARNER.lock().unwrap_or_else(PoisonError::into_inner) =
        Some(Learner::new(con.try_clone().map_err(|e| format!("harbor: {e}"))?));
    learn_again(changing_catalog());

    for _ in 0..configured_pool_size() {
        pool.push(con.try_clone().map_err(|e| format!("harbor: {e}"))?);
    }
    binder::keep_source(con.try_clone().map_err(|e| format!("harbor: {e}"))?);
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
        learner: None,
        probe: None,
        answering: Vec::new(),
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
/// executor per lease connection, the reaper, the learner, the threads that
/// answer cancels and the probe lane.
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
    let stop = Arc::clone(&r.stop);
    r.learner = Some(thread::Builder::new().name("harbor-learner".to_string()).spawn(move || learn_while_unknown(stop))?);

    static SERVING: AtomicU64 = AtomicU64::new(1);
    r.answering = spawn_answering(SERVING.fetch_add(1, Ordering::SeqCst))?;

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

/// Wait up to `patience` for `executors` to finish, putting the connections
/// they hand back in `pool`, and return those still running. The reaper
/// is gone by now, so its work is done here: a cancel the engine lost is
/// fired again each tick at a statement still running, as `cancel_expired`
/// does, or the stop would wait that statement out.
fn drain(
    executors: Vec<thread::JoinHandle<Option<Connection>>>,
    slots: &[Arc<SlotState>],
    pool: &mut Vec<Connection>,
    patience: Duration,
) -> Vec<thread::JoinHandle<Option<Connection>>> {
    let started = Instant::now();
    let mut fired = started;
    let mut pending = executors;
    loop {
        let (done, rest): (Vec<_>, Vec<_>) = pending.into_iter().partition(|h| h.is_finished());
        for h in done {
            if let Ok(Some(conn)) = h.join() {
                pool.push(conn);
            }
        }
        pending = rest;
        if pending.is_empty() || started.elapsed() >= patience {
            return pending;
        }
        if fired.elapsed() >= REAP_INTERVAL {
            fired = Instant::now();
            for slot in slots {
                if let Some(job) = slot.unheeded_job() {
                    slot.cancel(Some(job));
                }
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
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
    let mut pool = POOL.lock().unwrap();
    let pending = drain(r.workers.into_iter().chain(r.leases).collect(), &slots, &mut pool,
        Duration::from_secs(10));
    drop(slots);
    if !pending.is_empty() {
        eprintln!(
            "harbor: {} executor(s) still writing to stalled clients; abandoning them to checkpoint",
            pending.len()
        );
    }
    drop(pool);
    binder::stop();
    if let Some(h) = r.reaper {
        // Prompt: the reaper sleeps in short ticks and checks the stop flag.
        let _ = h.join();
    }
    if let Some(h) = r.learner {
        // It looks at the stop flag between short sleeps; a listing it runs
        // waits at most a SQLite file's own wait, and one waiting longer is
        // let go of.
        let started = Instant::now();
        while !h.is_finished() && started.elapsed() < Duration::from_secs(10) {
            thread::sleep(Duration::from_millis(10));
        }
        if h.is_finished() {
            let _ = h.join();
        }
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
    if let Some(c) = CONTROL.lock().unwrap().as_mut() {
        // The workspace records what is attached in it before the checkpoint
        // folds that in too.
        if workspace::serving() {
            workspace::record(c);
        }
        if let Err(e) = c.execute_batch("CHECKPOINT") {
            eprintln!(
                "harbor: shutdown CHECKPOINT failed ({e}); the WAL is intact and \
                 will replay on next open (no data lost, slower restart)"
            );
        }
    }

    // The cancels told by the statements drained are answered before the
    // server says it has stopped, and the process may leave.
    end_answering(r.answering, Duration::from_secs(10));

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

    // One gate before any routing: the declared length. A body declared
    // over the limit is refused here, so no handler reads or buffers any of
    // it, whatever route it names; what the client sends of it is discarded
    // by justhttp's drain, which is bounded in time and memory.
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
            // its rows may still be on their way, and they all arrive. One
            // that must wait for the engine's word is answered off this
            // thread (`Asked`).
            (Method::Delete, p) if p.starts_with("/sql/queries/") => {
                match cancel_query(&percent_decoded(&p["/sql/queries/".len()..])) {
                    // The probe lane hands its answer off too, so a client
                    // that does not read holds no lane.
                    Said::Now(cancelled) if exec.is_none() => {
                        return await_cancel(req, Arc::new(OnceLock::from(cancelled)), line);
                    }
                    Said::Now(cancelled) => answer_cancel(req, cancelled),
                    Said::Later(told) => return await_cancel(req, told, line),
                }
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
    /// A client's `ATTACH` or `DETACH` of a name: told, once the statement
    /// is over, whether it changed what that name is (`Claiming`).
    claims: Option<mpsc::SyncSender<bool>>,
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
        let job = Job { sql, params, shape, id: next_job_id(), deadline, reset: false, ready, body, claims: None };
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
    /// A listing met attached SQLite databases whose catalogs do not read
    /// for the moment (`catalog_listing`).
    Busy(Vec<Busy>),
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
        CatalogFailure::Busy(busy) => {
            let named: Vec<String> = busy.iter().map(|b| format!("{} ({}): {}", b.name, b.path, b.why)).collect();
            catalog_refuse(req, CatalogFailure::Refused(not_reading(&named)))
        }
    }
}

/// The refusal of a listing that does not read while the attached
/// databases `named`, each with why, do not.
fn not_reading(named: &[String]) -> Refusal {
    Refusal::sql(format!(
        "the catalog does not read while an attached database's does not, since the engine reads \
         every attached database's catalog together: {}. It reads again once that one does, or \
         once it is detached",
        named.join("; ")
    ))
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
/// quoted as SQL identifiers, including embedded double quotes, and every
/// table is named by its catalog too, since an attached database's tables
/// are counted in the same statement.
fn catalog_count_sql(tables: &[RelationKey]) -> Option<String> {
    if tables.is_empty() {
        return None;
    }
    let mut sql = String::from("SELECT table_ordinal, row_count FROM (");
    for (i, (database, schema, table)) in tables.iter().enumerate() {
        if i > 0 {
            sql.push_str(" UNION ALL ");
        }
        let _ = write!(
            sql,
            "SELECT {i}::UBIGINT AS table_ordinal, count(*)::UBIGINT AS row_count FROM {}",
            qualified(database, schema, table)
        );
    }
    sql.push_str(") AS exact_counts ORDER BY table_ordinal");
    Some(sql)
}

/// One table's exact count: `Ok(Err(..))` is the engine's refusal, which the
/// caller weighs (the table may be gone), and `Err` the failures every
/// catalog query shares.
fn count_alone(
    exec: Executor,
    peer: &justhttp::Peer,
    key: &RelationKey,
) -> Result<Result<u64, Refusal>, CatalogFailure> {
    let sql = catalog_count_sql(std::slice::from_ref(key)).unwrap_or_default();
    match catalog_rows(exec, peer, &sql) {
        Ok(rows) => match rows.first().and_then(|row| cell_opt_u64(row, 1)) {
            Some(count) => Ok(Ok(count)),
            None => Err(invalid_count_shape()),
        },
        Err(CatalogFailure::Refused(refusal)) if refusal.code == code::SQL_ERROR => Ok(Err(refusal)),
        Err(failure) => Err(failure),
    }
}

fn invalid_count_shape() -> CatalogFailure {
    CatalogFailure::Refused(Refusal {
        status: 500,
        code: code::INTERNAL,
        message: "the catalog row-count query returned an invalid shape".to_string(),
    })
}

/// Whether a table is listed, by (database, schema, name).
fn table_listed(exec: Executor, peer: &justhttp::Peer, (database, schema, name): &RelationKey) -> Result<bool, CatalogFailure> {
    let quote = wire::open::quote_literal;
    let sql = format!(
        "SELECT count(*) FROM duckdb_tables() WHERE database_name = {} AND schema_name = {} AND table_name = {}",
        quote(database), quote(schema), quote(name)
    );
    Ok(catalog_rows(exec, peer, &sql)?.first().and_then(|row| cell_opt_u64(row, 0)).unwrap_or(0) > 0)
}

/// A relation by (database, schema, name), as the catalog functions name it.
type RelationKey = (String, String, String);

/// `"database"."schema"."name"`.
fn qualified(database: &str, schema: &str, name: &str) -> String {
    let quote = wire::open::quote_ident;
    format!("{}.{}.{}", quote(database), quote(schema), quote(name))
}

/// A database the catalog describes: the served one, or one attached
/// beside it.
struct CatalogDatabase {
    name: String,
    path: Option<String>,
    storage: String,
    read_only: bool,
    served: bool,
    /// Detached by this read, its catalog not reading (`catalog_listing`).
    aside: bool,
}

/// The names the workspace's record holds, read on the served database;
/// None on any other server, and none when the record was dropped.
fn recorded_names(
    exec: Executor,
    peer: &justhttp::Peer,
    databases: &[CatalogDatabase],
) -> Result<Option<Vec<String>>, CatalogFailure> {
    if !workspace::serving() {
        return Ok(None);
    }
    let served = databases.iter().find(|d| d.served).map(|d| d.name.as_str()).unwrap_or_default();
    let sql = format!("SELECT name FROM {}", wire::open::record_in(served));
    match catalog_rows(exec, peer, &sql) {
        Ok(rows) => Ok(Some(rows.iter().map(|row| cell_str(row, 0)).collect())),
        Err(CatalogFailure::Refused(refusal)) if refusal.code == code::SQL_ERROR => Ok(Some(Vec::new())),
        Err(failure) => Err(failure),
    }
}

/// What is attached by none of `databases` and listed among them, holding
/// nothing, with why: what the workspace recorded and could not attach when
/// it started, each still in its record, and the SQLite files set aside
/// (`catalog_listing`), each still in the record when it was in it when set
/// aside. A file set aside whose name is attached again is forgotten.
fn unattached_now(
    exec: Executor,
    peer: &justhttp::Peer,
    databases: &[CatalogDatabase],
) -> Result<Vec<workspace::Unattached>, CatalogFailure> {
    let unattached = workspace::unattached();
    let aside = workspace::set_aside_now();
    if unattached.is_empty() && aside.is_empty() {
        return Ok(unattached);
    }
    let recorded = recorded_names(exec, peer, databases)?.unwrap_or_default();
    let attached = |name: &str| databases.iter().any(|d| !d.aside && d.name.eq_ignore_ascii_case(name));
    let mut listed: Vec<workspace::Unattached> = unattached
        .into_iter()
        .filter(|u| recorded.contains(&u.record.name))
        .filter(|u| !attached(&u.record.name))
        .collect();
    for a in aside {
        let name = &a.unattached.record.name;
        if attached(name) || (a.recorded && !recorded.contains(name)) {
            workspace::taken_back(name);
        } else if !listed.iter().any(|u| u.record.name.eq_ignore_ascii_case(name)) {
            listed.push(a.unattached);
        }
    }
    Ok(listed)
}

/// Why a SQLite file set aside is not attached, after the engine's reason,
/// when the server attached it from the workspace's record
/// ([`workspace::ours`]).
const SET_ASIDE_OURS: &str = "detached by the server, since an attached database whose catalog does not read \
                              fails every catalog read, and attached again by the first catalog read after it reads";
/// The same, for any other: a client's to attach again.
const SET_ASIDE: &str = "detached by the server, since an attached database whose catalog does not read \
                         fails every catalog read; attach it again once it reads";

/// Why an attached SQLite database is listed as the server last read it,
/// after the engine's reason (`catalog_stale`).
const STALE: &str = "its catalog does not read for the moment, and the engine reads every attached database's catalog \
                     together, so the whole document, every database in it, is the catalog the server last read, \
                     and may be stale; it is read anew once this one reads";

/// An attached SQLite database whose catalog does not read for the moment, and
/// the engine's reason: a lock another connection or program holds on its
/// file, or a failure not known to last (`for_good`).
#[derive(Debug, Clone)]
struct Busy {
    name: String,
    path: String,
    why: String,
}

/// The attached SQLite databases the last catalog read found busy, asked
/// first by the next one (`still_busy`).
static BUSY: Mutex<Vec<Busy>> = Mutex::new(Vec::new());

/// The last catalog each style answered in full, for a read that meets a
/// busy database (`catalog_stale`).
static LAST_CATALOG: Mutex<(Option<Catalog>, Option<Inventory>)> = Mutex::new((None, None));

/// Whether a SQLite file's catalog does not read for good, as far as the
/// server can tell from `why`, the engine's reason: its file is gone, or
/// the engine finds it is not a SQLite database or cannot open it. A lock
/// passes, and so may anything else.
fn for_good(path: &str, why: &str) -> bool {
    let in_memory = path.is_empty() || path == ":memory:";
    (!in_memory && !std::path::Path::new(path).is_file())
        || ["file is not a database", "malformed", "unable to open database file"].iter().any(|m| why.contains(m))
}

/// One of the catalog functions' listings, which read every attached
/// database's catalog at once: one database whose catalog does not read
/// fails the listing of all of them, and the engine names the statement it
/// sent the database and not the database. A SQLite file that is not
/// SQLite, or that a connection or another program holds locked, does
/// that, and attaching one reads nothing of it; and the engine lists no
/// database's catalog alone. So a listing that fails asks each attached
/// SQLite database alone. One that does not read for good (`for_good`) is
/// detached and set aside, with its reason, as `aside` among `databases`,
/// and the listing runs again; a later read attaches it again once it
/// reads when the server attached it from the workspace's record
/// (`take_back`). One that is busy is left as it is, attached, and the
/// listing fails as `Busy`, answered from the catalog last read
/// (`catalog_stale`). One that does not wait on a lock is not slowed. One
/// that cannot be detached fails the listing, named.
fn catalog_listing(
    exec: Executor,
    peer: &justhttp::Peer,
    sql: &str,
    databases: &mut [CatalogDatabase],
) -> Result<Vec<Vec<serde_json::Value>>, CatalogFailure> {
    let refusal = match catalog_rows(exec, peer, sql) {
        Err(CatalogFailure::Refused(refusal)) if refusal.code == code::SQL_ERROR => refusal,
        other => return other,
    };
    let mut recorded: Option<Option<Vec<String>>> = None;
    let mut unreadable = Vec::new();
    let mut busy = Vec::new();
    let mut set_aside = false;
    for i in 0..databases.len() {
        let database = &databases[i];
        if database.served || database.aside || database.storage != "sqlite" {
            continue;
        }
        let path = database.path.clone().unwrap_or_default();
        let why = match reads_alone(exec, peer, &database.name, &path)? {
            Ok(()) => continue,
            Err(why) => why,
        };
        if !for_good(&path, &why) {
            busy.push(Busy { name: database.name.clone(), path, why });
            continue;
        }
        let detach = format!("DETACH {}", wire::open::quote_ident(&database.name));
        match catalog_rows(exec, peer, &detach) {
            Ok(_) => {}
            // Another catalog read set it aside first.
            Err(CatalogFailure::Refused(_))
                if workspace::set_aside_now().iter().any(|a| a.unattached.record.name == database.name) =>
            {
                databases[i].aside = true;
                set_aside = true;
                continue;
            }
            Err(CatalogFailure::Refused(refusal)) if refusal.code == code::SQL_ERROR => {
                unreadable.push(format!("{} ({path}): {why}, and it was not detached: {}", database.name, refusal.message));
                continue;
            }
            Err(failure) => return Err(failure),
        }
        if recorded.is_none() {
            recorded = Some(recorded_names(exec, peer, databases)?);
        }
        let database = &mut databases[i];
        database.aside = true;
        set_aside = true;
        eprintln!("harbor: {} ({path}) is detached, since its catalog does not read: {why}", database.name);
        let record = wire::open::Record {
            name: database.name.clone(),
            path,
            storage: database.storage.clone(),
            read_only: database.read_only,
        };
        let in_record = recorded.as_ref().and_then(|r| r.as_ref()).is_some_and(|r| r.contains(&record.name));
        let ours = in_record && workspace::ours(&record.name);
        let reason = format!("{why}: {}", if ours { SET_ASIDE_OURS } else { SET_ASIDE });
        workspace::set_aside(workspace::SetAside {
            unattached: workspace::Unattached { record, reason },
            recorded: in_record,
            ours,
        });
    }
    if !unreadable.is_empty() {
        return Err(CatalogFailure::Refused(not_reading(&unreadable)));
    }
    if set_aside {
        match catalog_rows(exec, peer, sql) {
            Err(CatalogFailure::Refused(refusal)) if refusal.code == code::SQL_ERROR && !busy.is_empty() => {}
            other => return other,
        }
    }
    if busy.is_empty() {
        return Err(CatalogFailure::Refused(refusal));
    }
    for b in &busy {
        eprintln!("harbor: {} ({}) does not read for the moment, and stays attached: {}", b.name, b.path, b.why);
    }
    *BUSY.lock().unwrap() = busy.clone();
    Err(CatalogFailure::Busy(busy))
}

/// The databases the last catalog read found busy that still are, asked
/// alone (`reads_alone`) before the listings, so a read while one stays
/// busy does not wait out its lock again: none when each reads, or is gone.
fn still_busy(exec: Executor, peer: &justhttp::Peer, databases: &[CatalogDatabase]) -> Result<Vec<Busy>, CatalogFailure> {
    let known = BUSY.lock().unwrap().clone();
    let mut busy = Vec::new();
    for b in known {
        let attached = databases.iter().any(|d| !d.served && d.name == b.name && d.path.as_deref().unwrap_or_default() == b.path);
        if !attached {
            continue;
        }
        if let Err(why) = reads_alone(exec, peer, &b.name, &b.path)?
            && !for_good(&b.path, &why)
        {
            busy.push(Busy { why, ..b });
        }
    }
    if busy.is_empty() {
        BUSY.lock().unwrap().clear();
    }
    Ok(busy)
}

/// An attached database's entry in either style, as a document answered
/// from the one last read (`catalog_stale`) rewrites it.
trait AttachedEntry: Clone {
    fn key(&self) -> (&str, Option<&str>, bool);
    fn error_mut(&mut self) -> &mut Option<String>;
    fn holding_nothing(name: String, path: Option<String>, storage: String, read_only: bool, error: Option<String>) -> Self;
}

impl AttachedEntry for Attached {
    fn key(&self) -> (&str, Option<&str>, bool) {
        (&self.name, self.path.as_deref(), self.error.is_none())
    }
    fn error_mut(&mut self) -> &mut Option<String> {
        &mut self.error
    }
    fn holding_nothing(name: String, path: Option<String>, storage: String, read_only: bool, error: Option<String>) -> Self {
        Attached { name, path, storage, read_only, error, ..Attached::default() }
    }
}

impl AttachedEntry for AttachedInventory {
    fn key(&self) -> (&str, Option<&str>, bool) {
        (&self.name, self.path.as_deref(), self.error.is_none())
    }
    fn error_mut(&mut self) -> &mut Option<String> {
        &mut self.error
    }
    fn holding_nothing(name: String, path: Option<String>, storage: String, read_only: bool, error: Option<String>) -> Self {
        AttachedInventory { name, path, storage, read_only, error, tables: Vec::new(), views: Vec::new() }
    }
}

/// The attached databases of a document answered from `last`, the one last
/// read: each database attached at this read, as `last` lists it when it lists it
/// attached from the same path, holding nothing when it does not; each busy
/// one with why (`STALE`); then what is unattached at this read, with why.
fn stale_attached<E: AttachedEntry>(
    last: &[E],
    databases: &[CatalogDatabase],
    busy: &[Busy],
    unattached: Vec<workspace::Unattached>,
) -> Vec<E> {
    let mut attached: Vec<E> = Vec::new();
    for d in databases.iter().filter(|d| !d.served && !d.aside) {
        let mut entry = last
            .iter()
            .find(|e| e.key() == (d.name.as_str(), d.path.as_deref(), true))
            .cloned()
            .unwrap_or_else(|| E::holding_nothing(d.name.clone(), d.path.clone(), d.storage.clone(), d.read_only, None));
        if let Some(b) = busy.iter().find(|b| b.name == d.name) {
            *entry.error_mut() = Some(format!("{}: {STALE}", b.why));
        }
        attached.push(entry);
    }
    attached.extend(unattached.into_iter().map(|u| {
        E::holding_nothing(u.record.name, Some(u.record.path), u.record.storage, u.record.read_only, Some(u.reason))
    }));
    attached.sort_by(|a, b| a.key().0.cmp(b.key().0));
    attached
}

/// Answer a catalog read that met `busy`, attached SQLite databases whose
/// catalogs do not read for the moment: the engine reads every attached
/// database's catalog together, so nothing of the catalog lists until they
/// read, and detaching one would take it from every connection using it,
/// the server's own among them, as a long write to it holds its lock. So
/// the read is answered with the catalog the server last read in the same
/// style (the lite one from the full one when it has read only that), its
/// `stale` saying so (`stale_note`), each busy database's entry saying why
/// (`STALE`), the databases attached and
/// unattached as this read finds them, and the sizes as it stats them. With no
/// catalog read yet, the read is refused, naming each busy database.
fn catalog_stale(
    req: Request,
    exec: Executor,
    peer: &justhttp::Peer,
    style: CatalogStyle,
    databases: &[CatalogDatabase],
    busy: Vec<Busy>,
) -> (bool, u16) {
    let (full, lite) = LAST_CATALOG.lock().unwrap().clone();
    let lite = match (&style, lite, &full) {
        (CatalogStyle::Lite, None, Some(full)) => Some(inventory_of(full)),
        (_, lite, _) => lite,
    };
    let refused = matches!((&style, &full, &lite), (CatalogStyle::Full, None, _) | (CatalogStyle::Lite, _, None));
    if refused {
        return catalog_refuse(req, CatalogFailure::Busy(busy));
    }
    let unattached = match unattached_now(exec, peer, databases) {
        Ok(unattached) => unattached,
        Err(failure) => return catalog_refuse(req, failure),
    };
    let (database_size_bytes, wal_size_bytes) = database_disk_sizes();
    let stale = Some(stale_note(&busy));
    let body = match style {
        CatalogStyle::Full => {
            let mut catalog = full.unwrap_or_default();
            catalog.attached = stale_attached(&catalog.attached, databases, &busy, unattached);
            (catalog.database_size_bytes, catalog.wal_size_bytes) = (database_size_bytes, wal_size_bytes);
            catalog.stale = stale;
            serde_json::to_string(&catalog).unwrap()
        }
        CatalogStyle::Lite => {
            let Some(mut inventory) = lite else { return catalog_refuse(req, CatalogFailure::Busy(busy)) };
            inventory.attached = stale_attached(&inventory.attached, databases, &busy, unattached);
            (inventory.database_size_bytes, inventory.wal_size_bytes) = (database_size_bytes, wal_size_bytes);
            inventory.stale = stale;
            serde_json::to_string(&inventory).unwrap()
        }
    };
    let _ = req.respond(json_response(200, &body));
    (true, 200)
}

/// What a document answered as last read says of itself (`stale`): which
/// attached databases do not read.
fn stale_note(busy: &[Busy]) -> String {
    let named: Vec<String> = busy.iter().map(|b| format!("{} ({})", b.name, b.path)).collect();
    format!(
        "this is the catalog the server last read, since the catalog of {} does not read for the moment, and the \
         engine reads every attached database's together: every database in it may be stale, and it is read anew \
         once that one reads",
        named.join(", ")
    )
}

/// The lite style of a full catalog: what it names.
fn inventory_of(catalog: &Catalog) -> Inventory {
    let named = |schema: &str, name: &str| Named { name: name.to_string(), schema: schema.to_string() };
    let tables = |tables: &[Table]| tables.iter().map(|t| named(&t.schema, &t.name)).collect::<Vec<_>>();
    let views = |views: &[View]| views.iter().map(|v| named(&v.schema, &v.name)).collect::<Vec<_>>();
    Inventory {
        harbor_version: catalog.harbor_version.clone(),
        duckdb_version: catalog.duckdb_version.clone(),
        database_size_bytes: catalog.database_size_bytes,
        wal_size_bytes: catalog.wal_size_bytes,
        tables: tables(&catalog.tables),
        views: views(&catalog.views),
        attached: catalog
            .attached
            .iter()
            .map(|a| AttachedInventory {
                name: a.name.clone(),
                path: a.path.clone(),
                storage: a.storage.clone(),
                read_only: a.read_only,
                error: a.error.clone(),
                tables: tables(&a.tables),
                views: views(&a.views),
            })
            .collect(),
        stale: catalog.stale.clone(),
    }
}

/// Whether the SQLite file at `path`, attached as `name`, reads its own
/// catalog, and the engine's reason when it does not. Asked on an
/// attachment of its own, hidden from the catalog functions and waiting
/// [`PROBE_BUSY_MS`] on a lock, so the catalog read that asks does not wait
/// out the file's own wait a second time; an in-memory one, on its own
/// attachment.
fn reads_alone(
    exec: Executor,
    peer: &justhttp::Peer,
    name: &str,
    path: &str,
) -> Result<Result<(), String>, CatalogFailure> {
    let quote = wire::open::quote_literal;
    let read = |name: &str| format!("SELECT count(*) FROM sqlite_query({}, 'SELECT count(*) FROM sqlite_master')", quote(name));
    let why = |refusal: Refusal| refusal.message.split("\nLINE ").next().unwrap_or_default().trim().to_string();
    if path.is_empty() || path == ":memory:" {
        return match catalog_rows(exec, peer, &read(name)) {
            Ok(_) => Ok(Ok(())),
            Err(CatalogFailure::Refused(refusal)) => Ok(Err(why(refusal))),
            Err(failure) => Err(failure),
        };
    }
    let probe = format!("{PROBE}{}", next_job_id());
    let attach = format!(
        "ATTACH {} AS {} (TYPE sqlite, READ_ONLY, BUSY_TIMEOUT {PROBE_BUSY_MS}, HIDDEN)",
        quote(path),
        wire::open::quote_ident(&probe)
    );
    match catalog_rows(exec, peer, &attach) {
        Ok(_) => {}
        Err(CatalogFailure::Refused(refusal)) => return Ok(Err(why(refusal))),
        Err(failure) => return Err(failure),
    }
    let read = catalog_rows(exec, peer, &read(&probe));
    let _ = catalog_rows(exec, peer, &format!("DETACH {}", wire::open::quote_ident(&probe)));
    match read {
        Ok(_) => Ok(Ok(())),
        Err(CatalogFailure::Refused(refusal)) => Ok(Err(why(refusal))),
        Err(failure) => Err(failure),
    }
}

/// How long the question whether a SQLite file reads waits on a lock, in
/// milliseconds: long enough to ride over a writer's commit.
const PROBE_BUSY_MS: u32 = 100;

/// Attach again each SQLite file set aside that the server attached from
/// the workspace's record ([`workspace::ours`]) and whose file reads
/// (`reads_alone`), under its name, as its record attaches it, so one still
/// not reading costs a catalog read a few milliseconds. One whose row the
/// record no longer holds is forgotten, not attached. Any other is a
/// client's, attached with the client's options, and is left for a client
/// to attach again.
fn take_back(exec: Executor, peer: &justhttp::Peer) -> Result<(), CatalogFailure> {
    let aside = workspace::set_aside_now();
    if aside.is_empty() {
        return Ok(());
    }
    let recorded = match workspace::serving() {
        true => {
            let served = catalog_rows(exec, peer, "SELECT current_database()")?;
            let served = served.first().map(|row| cell_str(row, 0)).unwrap_or_default();
            let databases = [CatalogDatabase {
                name: served,
                path: None,
                storage: String::new(),
                read_only: false,
                served: true,
                aside: false,
            }];
            recorded_names(exec, peer, &databases)?
        }
        false => None,
    };
    for a in aside {
        let record = &a.unattached.record;
        if a.recorded && recorded.as_ref().is_some_and(|r| !r.contains(&record.name)) {
            workspace::taken_back(&record.name);
            continue;
        }
        if !a.ours {
            continue;
        }
        // A file gone is not attached, since attaching a missing path makes
        // an empty database there.
        if !std::path::Path::new(&record.path).is_file() || reads_alone(exec, peer, &record.name, &record.path)?.is_err() {
            continue;
        }
        // A client's DETACH of its name since forgot it, and one of its
        // name that has not yet been claimed may.
        let taking = workspace::taking();
        if !workspace::set_aside_now().iter().any(|s| s.unattached.record.name == record.name)
            || workspace::is_pending(&record.name, &taking)
        {
            continue;
        }
        match catalog_rows(exec, peer, &record.attach_sql()) {
            Ok(_) => {
                eprintln!("harbor: {} ({}) reads again, and is attached again", record.name, record.path);
                workspace::taken_back(&record.name);
            }
            Err(CatalogFailure::Refused(_)) => {}
            Err(failure) => return Err(failure),
        }
    }
    Ok(())
}

/// Bind the view a `duckdb_views()` row names (database, schema, name,
/// sql): None when it reads, else why it does not. A client that leaves
/// ends the catalog read.
fn bind_view(peer: &justhttp::Peer, row: &[serde_json::Value]) -> Result<Option<String>, CatalogFailure> {
    let key: RelationKey = (cell_str(row, 0), cell_str(row, 1), cell_str(row, 2));
    let probe = format!("SELECT * FROM {} LIMIT 0", qualified(&key.0, &key.1, &key.2));
    match binder::bind(peer, &key, &cell_str(row, 3), probe) {
        Ok(Ok(())) => Ok(None),
        Ok(Err(refusal)) => match view_error(&refusal) {
            Some(error) => Ok(Some(error)),
            None => Err(CatalogFailure::Refused(refusal)),
        },
        Err(binder::Departed) => Err(CatalogFailure::Refused(Refusal::cancelled())),
    }
}

/// Why a view does not read, from the refusal its probe met, or None when
/// the refusal is not the view's to carry: a cancel, a client gone, a
/// server stopping. The engine's message stands as written, less any
/// `LINE` excerpt, which would quote the probe and not the view.
fn view_error(refusal: &Refusal) -> Option<String> {
    if refusal.code != code::SQL_ERROR {
        return None;
    }
    let message = refusal.message.split("\nLINE ").next().unwrap_or_default();
    Some(message.trim().to_string())
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
/// `?style=lite` answers the inventory alone: the versions, the sizes, each
/// table's and view's name and schema, and each attached database with its
/// tables and views named the same way — enough to draw a database list without
/// paying for counts, columns, constraints, indexes, DDL, or sequences, in
/// queries here or in bytes on the wire. It is the same document family at
/// lower fidelity, not a second contract: a field a style omits is absent,
/// never differently shaped.
///
/// Views come with their columns and their DDL and without a count, since
/// counting a view over a file reads the file. Each view is bound before
/// the columns are read (`SELECT * … LIMIT 0`, which reads what binding a
/// reader reads: a CSV's sample, a Parquet footer, the whole of a GeoJSON
/// file, for GDAL parses it to open it), so its columns are the ones it
/// reads as when the catalog is asked; the engine keeps the shape a view
/// last bound to and reports that otherwise. A view that does not bind, its file gone or its
/// extension not loaded, is listed with no columns and the engine's reason,
/// and the rest of the document is unaffected. Views are bound off the pool,
/// each within a budget, and a view over one file only when the file
/// changed (`binder`).
///
/// Every database attached beside the served one is listed under
/// `attached`, `system` and `temp` never, with its path, its storage type
/// and whether it is read-only, and its tables and views in the same
/// shapes. A DuckDB catalog's tables are counted exactly with the served
/// database's, in the one statement; another engine's are not counted,
/// since a count there reads every row of the file. On the workspace, a
/// file it records and could not attach at its start is listed there too,
/// holding nothing, with why in `error`, and the record itself, in the
/// `_harbor` schema, is not listed (`workspace`). An attached SQLite file
/// whose own catalog does not read fails every listing: one that does not
/// read for good is detached and listed there the same way, and attached
/// again by a later read once it reads when the workspace attached it
/// (`catalog_listing`, `take_back`); one that is busy stays attached, and
/// the read is answered with the catalog last read, the busy one saying
/// why and the document's `stale` saying so (`catalog_stale`).
///
/// Ordering is part of the contract: tables and views by (schema, name),
/// attached databases by name, columns in ordinal position, indexes and
/// sequences by name, unique constraints by their column lists, foreign keys
/// by their referenced table and column lists. A stable database answers
/// with byte-identical output.
fn run_catalog(req: Request, exec: Executor) -> (bool, u16) {
    let peer = req.peer();
    let style = match catalog_style(req.url()) {
        Ok(style) => style,
        Err(message) => {
            let _ = req.respond(error_response(400, code::BAD_REQUEST, &message));
            return (true, 400);
        }
    };
    let version_rows = match catalog_rows(exec, &peer, "SELECT library_version FROM pragma_version()") {
        Ok(rows) => rows,
        Err(failure) => return catalog_refuse(req, failure),
    };
    if let Err(failure) = take_back(exec, &peer) {
        return catalog_refuse(req, failure);
    }
    // `system` and `temp` are the internal databases; every other one is the
    // served database, which current_database() names, or one attached
    // beside it. Relations of any database not in this list are passed over
    // below, wherever a catalog function reports them.
    let database_rows = match catalog_rows(
        exec, &peer,
        "SELECT database_name, path, type, readonly, database_name = current_database() \
         FROM duckdb_databases() WHERE NOT internal ORDER BY database_name",
    ) {
        Ok(rows) => rows,
        Err(failure) => return catalog_refuse(req, failure),
    };
    let mut databases: Vec<CatalogDatabase> = database_rows
        .iter()
        .map(|row| CatalogDatabase {
            name: cell_str(row, 0),
            path: cell_opt_str(row, 1).filter(|p| !p.is_empty()),
            storage: cell_str(row, 2),
            read_only: cell_bool(row, 3),
            served: cell_bool(row, 4),
            aside: false,
        })
        .collect();
    let database_of: HashMap<String, usize> =
        databases.iter().enumerate().map(|(i, d)| (d.name.clone(), i)).collect();
    // A database the last read found busy that still is answers this one
    // from the catalog last read, without waiting out its lock again.
    match still_busy(exec, &peer, &databases) {
        Ok(busy) if !busy.is_empty() => return catalog_stale(req, exec, &peer, style, &databases, busy),
        Ok(_) => {}
        Err(failure) => return catalog_refuse(req, failure),
    }
    let table_sql = match style {
        CatalogStyle::Full => {
            "SELECT database_name, schema_name, table_name, sql FROM duckdb_tables() \
             WHERE NOT internal AND NOT temporary \
             ORDER BY database_name, schema_name, table_name"
        }
        CatalogStyle::Lite => {
            "SELECT database_name, schema_name, table_name FROM duckdb_tables() \
             WHERE NOT internal AND NOT temporary \
             ORDER BY database_name, schema_name, table_name"
        }
    };
    let table_rows = match catalog_listing(exec, &peer, table_sql, &mut databases) {
        Ok(rows) => rows,
        Err(CatalogFailure::Busy(busy)) => return catalog_stale(req, exec, &peer, style, &databases, busy),
        Err(failure) => return catalog_refuse(req, failure),
    };
    let view_rows = match catalog_listing(
        exec, &peer,
        "SELECT database_name, schema_name, view_name, sql FROM duckdb_views() \
         WHERE NOT internal AND NOT temporary \
         ORDER BY database_name, schema_name, view_name",
        &mut databases,
    ) {
        Ok(rows) => rows,
        Err(CatalogFailure::Busy(busy)) => return catalog_stale(req, exec, &peer, style, &databases, busy),
        Err(failure) => return catalog_refuse(req, failure),
    };
    // The workspace's record of what is attached in it is Harbor's, and is
    // not listed among the user's relations.
    let hidden = workspace::serving();
    let listed = |row: &Vec<serde_json::Value>| match database_of.get(&cell_str(row, 0)) {
        Some(&i) => !(hidden && databases[i].served && cell_str(row, 1) == wire::open::RECORD_SCHEMA),
        None => false,
    };
    let table_rows: Vec<_> = table_rows.into_iter().filter(listed).collect();
    let view_rows: Vec<_> = view_rows.into_iter().filter(listed).collect();
    let duckdb_version = version_rows.first().map(|r| cell_str(r, 0)).unwrap_or_default();
    let harbor_version = env!("CARGO_PKG_VERSION").to_string();
    let (database_size_bytes, wal_size_bytes) = database_disk_sizes();

    // The lite style stops here: everything it answers is already in hand,
    // and the count, the views' binding and the four shape queries below
    // never run.
    if let CatalogStyle::Lite = style {
        let unattached = match unattached_now(exec, &peer, &databases) {
            Ok(unattached) => unattached,
            Err(failure) => return catalog_refuse(req, failure),
        };
        let named_in = |rows: &[Vec<serde_json::Value>], database: &str| -> Vec<Named> {
            rows.iter()
                .filter(|row| cell_str(row, 0) == database)
                .map(|row| Named { name: cell_str(row, 2), schema: cell_str(row, 1) })
                .collect()
        };
        let served = databases.iter().find(|d| d.served).map(|d| d.name.as_str()).unwrap_or_default();
        let mut attached: Vec<AttachedInventory> = databases
            .iter()
            .filter(|d| !d.served && !d.aside)
            .map(|d| AttachedInventory {
                name: d.name.clone(),
                path: d.path.clone(),
                storage: d.storage.clone(),
                read_only: d.read_only,
                error: None,
                tables: named_in(&table_rows, &d.name),
                views: named_in(&view_rows, &d.name),
            })
            .collect();
        attached.extend(unattached.into_iter().map(|u| AttachedInventory {
            name: u.record.name,
            path: Some(u.record.path),
            storage: u.record.storage,
            read_only: u.record.read_only,
            error: Some(u.reason),
            tables: Vec::new(),
            views: Vec::new(),
        }));
        attached.sort_by(|a, b| a.name.cmp(&b.name));
        let inventory = Inventory {
            harbor_version,
            duckdb_version,
            database_size_bytes,
            wal_size_bytes,
            tables: named_in(&table_rows, served),
            views: named_in(&view_rows, served),
            attached,
            stale: None,
        };
        LAST_CATALOG.lock().unwrap().1 = Some(inventory.clone());
        BUSY.lock().unwrap().clear();
        let _ = req.respond(json_response(200, &serde_json::to_string(&inventory).unwrap()));
        learn_after_reading();
        return (true, 200);
    }

    // SQL cannot turn values returned by duckdb_tables() into relation
    // identifiers. Build those identifiers here, where they can be quoted
    // exactly, then let one UNION ALL statement count every table under one
    // query snapshot. COUNT(*) projects no application columns; it reads the
    // engine's visibility information and therefore excludes deleted rows
    // that the physical storage cardinality still includes. Only DuckDB's
    // own storage is counted: another engine's count reads its whole file.
    let key_of = |row: &Vec<serde_json::Value>| -> RelationKey { (cell_str(row, 0), cell_str(row, 1), cell_str(row, 2)) };
    let counted: Vec<usize> = (0..table_rows.len())
        .filter(|&i| databases[database_of[&cell_str(&table_rows[i], 0)]].storage == "duckdb")
        .collect();
    let counted_keys: Vec<RelationKey> = counted.iter().map(|&i| key_of(&table_rows[i])).collect();
    // A table can be dropped, or its database detached, between the listing
    // and the count, by any other client; the one statement then fails for
    // all of them. Each is then counted alone, and a count that fails is set
    // aside until the relations are listed again at the end: a table gone by
    // then is left out of the document, and one listed again is counted
    // again.
    let mut row_counts: Vec<Option<u64>> = vec![None; table_rows.len()];
    let mut count_failures: Vec<usize> = Vec::new();
    if let Some(sql) = catalog_count_sql(&counted_keys) {
        match catalog_rows(exec, &peer, &sql) {
            Ok(rows) => {
                if rows.len() != counted.len() {
                    return catalog_refuse(req, invalid_count_shape());
                }
                for (ordinal, (row, &i)) in rows.iter().zip(&counted).enumerate() {
                    if cell_opt_u64(row, 0) != Some(ordinal as u64) {
                        return catalog_refuse(req, invalid_count_shape());
                    }
                    let Some(count) = cell_opt_u64(row, 1) else { return catalog_refuse(req, invalid_count_shape()) };
                    row_counts[i] = Some(count);
                }
            }
            Err(CatalogFailure::Refused(refusal)) if refusal.code == code::SQL_ERROR => {
                for (&i, key) in counted.iter().zip(&counted_keys) {
                    match count_alone(exec, &peer, key) {
                        Ok(Ok(count)) => row_counts[i] = Some(count),
                        Ok(Err(_)) => count_failures.push(i),
                        Err(failure) => return catalog_refuse(req, failure),
                    }
                }
            }
            Err(failure) => return catalog_refuse(req, failure),
        }
    }

    // Each view bound, one statement apiece, so one that does not bind
    // costs only itself, and off the pool, each within a budget (`binder`).
    // Binding comes before the columns are read: the engine records the
    // shape a view binds to, so duckdb_columns() then answers with the
    // shape each view reads as at this moment.
    let mut view_errors: Vec<Option<String>> = Vec::with_capacity(view_rows.len());
    for row in &view_rows {
        match bind_view(&peer, row) {
            Ok(error) => view_errors.push(error),
            Err(failure) => return catalog_refuse(req, failure),
        }
    }

    let column_rows = match catalog_listing(
        exec, &peer,
        "SELECT database_name, schema_name, table_name, column_name, data_type, is_nullable, \
                column_default, is_generated, generation_expression \
         FROM duckdb_columns() \
         WHERE NOT internal \
         ORDER BY database_name, schema_name, table_name, column_index",
        &mut databases,
    ) {
        Ok(rows) => rows,
        Err(CatalogFailure::Busy(busy)) => return catalog_stale(req, exec, &peer, style, &databases, busy),
        Err(failure) => return catalog_refuse(req, failure),
    };
    let constraint_rows = match catalog_listing(
        exec, &peer,
        "SELECT database_name, schema_name, table_name, constraint_type, constraint_column_names, \
                referenced_table, referenced_column_names \
         FROM duckdb_constraints() \
         WHERE constraint_type IN ('PRIMARY KEY', 'UNIQUE', 'FOREIGN KEY') \
         ORDER BY database_name, schema_name, table_name, constraint_index",
        &mut databases,
    ) {
        Ok(rows) => rows,
        Err(CatalogFailure::Busy(busy)) => return catalog_stale(req, exec, &peer, style, &databases, busy),
        Err(failure) => return catalog_refuse(req, failure),
    };
    // duckdb_indexes() lists only the indexes CREATE INDEX made; the internal
    // ART indexes that implement PRIMARY KEY and UNIQUE column constraints are
    // not in it, which is exactly the distinction the contract wants — that
    // constraint-borne uniqueness travels in uniqueConstraints above, not here.
    let index_rows = match catalog_listing(
        exec, &peer,
        "SELECT database_name, schema_name, table_name, index_name, is_unique, expressions \
         FROM duckdb_indexes() \
         ORDER BY database_name, schema_name, table_name, index_name",
        &mut databases,
    ) {
        Ok(rows) => rows,
        Err(CatalogFailure::Busy(busy)) => return catalog_stale(req, exec, &peer, style, &databases, busy),
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
    // load-bearing — and looked up by (database, schema, name), never
    // iterated from the map, so nothing about the output depends on hash
    // order. A table and a view cannot share a name in one schema, so one map
    // finds either.
    enum Relation {
        Table(usize),
        View(usize),
    }
    let mut tables: Vec<Table> = Vec::new();
    let mut views: Vec<View> = Vec::new();
    let mut index_of: HashMap<RelationKey, Relation> = HashMap::new();
    for (row, row_count) in table_rows.iter().zip(row_counts) {
        let (_, schema, name) = key_of(row);
        index_of.insert(key_of(row), Relation::Table(tables.len()));
        tables.push(Table { schema, name, row_count, ddl: cell_opt_str(row, 3), ..Table::default() });
    }
    for (row, error) in view_rows.iter().zip(view_errors) {
        let (_, schema, name) = key_of(row);
        index_of.insert(key_of(row), Relation::View(views.len()));
        views.push(View { schema, name, columns: Vec::new(), error, ddl: cell_opt_str(row, 3) });
    }
    let table_at = |row: &Vec<serde_json::Value>| match index_of.get(&key_of(row)) {
        Some(&Relation::Table(t)) => Some(t),
        _ => None,
    };
    for row in &column_rows {
        let column = Column {
            name: cell_str(row, 3),
            duck_type: cell_str(row, 4),
            not_null: !cell_bool(row, 5),
            default: cell_opt_str(row, 6),
            generated: cell_bool(row, 7),
            generation_expression: cell_opt_str(row, 8),
            primary: false,
        };
        match index_of.get(&key_of(row)) {
            Some(&Relation::Table(t)) => tables[t].columns.push(column),
            Some(&Relation::View(v)) => views[v].columns.push(column),
            None => {}
        }
    }
    for row in &constraint_rows {
        let Some(t) = table_at(row) else { continue };
        let columns = cell_list(row, 4);
        match cell_str(row, 3).as_str() {
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
                    ref_table: cell_str(row, 5),
                    ref_schema,
                    ref_columns: cell_list(row, 6),
                });
            }
            _ => {}
        }
    }
    for row in &index_rows {
        let Some(t) = table_at(row) else { continue };
        let (mut columns, mut expressions) = (Vec::new(), Vec::new());
        for part in index_parts(&cell_str(row, 5)) {
            match part {
                IndexPart::Column(name) => columns.push(name),
                IndexPart::Expression(text) => expressions.push(text),
            }
        }
        tables[t].indexes.push(Index {
            name: cell_str(row, 3),
            columns,
            expressions,
            unique: cell_bool(row, 4),
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

    // Listed again, last: what another client dropped or detached while the
    // queries above ran is left out, rather than shown half described or
    // failing the whole document. A table whose count failed and that is
    // still here fails it with the count's own error.
    let present_rows = match catalog_listing(
        exec, &peer,
        "SELECT database_name, schema_name, table_name FROM duckdb_tables() \
         WHERE NOT internal AND NOT temporary \
         UNION ALL SELECT database_name, schema_name, view_name FROM duckdb_views() \
         WHERE NOT internal AND NOT temporary \
         UNION ALL SELECT database_name, NULL, NULL FROM duckdb_databases() WHERE NOT internal",
        &mut databases,
    ) {
        Ok(rows) => rows,
        Err(CatalogFailure::Busy(busy)) => return catalog_stale(req, exec, &peer, style, &databases, busy),
        Err(failure) => return catalog_refuse(req, failure),
    };
    let mut present: std::collections::HashSet<RelationKey> = present_rows.iter().map(key_of).collect();
    // A table whose count failed and that is listed again — its
    // database detached and attached again, say — is counted again, and
    // looked for again when that fails too. Three tries: a table still
    // listed after three failed counts fails the document with the engine's
    // error, as the one statement would have for a table that cannot be
    // counted.
    for i in count_failures {
        let key = key_of(&table_rows[i]);
        let mut tries = 0;
        while present.contains(&key) {
            tries += 1;
            match count_alone(exec, &peer, &key) {
                Ok(Ok(count)) => {
                    tables[i].row_count = Some(count);
                    break;
                }
                Ok(Err(refusal)) if tries >= 3 => return catalog_refuse(req, CatalogFailure::Refused(refusal)),
                Ok(Err(_)) => match table_listed(exec, &peer, &key) {
                    Ok(true) => {}
                    Ok(false) => {
                        present.remove(&key);
                    }
                    Err(failure) => return catalog_refuse(req, failure),
                },
                Err(failure) => return catalog_refuse(req, failure),
            }
        }
    }
    // A table always has a column, so one with none was gone when the
    // columns were read.
    for (i, table) in tables.iter().enumerate() {
        if table.columns.is_empty() {
            present.remove(&key_of(&table_rows[i]));
        }
    }
    // A view whose binding failed and that is listed still is bound once
    // more, since what failed may have been its database, detached and
    // attached again while it was bound. One that still does not read has
    // no columns: the ones the engine kept are the shape it last read as,
    // not one it reads as.
    for (row, view) in view_rows.iter().zip(views.iter_mut()) {
        if view.error.is_none() || !present.contains(&key_of(row)) {
            continue;
        }
        match bind_view(&peer, row) {
            Ok(None) => view.error = None,
            Ok(error) => {
                view.error = error;
                view.columns.clear();
            }
            Err(failure) => return catalog_refuse(req, failure),
        }
    }
    let still_attached = |name: &str| present.contains(&(name.to_string(), String::new(), String::new()));
    let unattached = match unattached_now(exec, &peer, &databases) {
        Ok(unattached) => unattached,
        Err(failure) => return catalog_refuse(req, failure),
    };

    // Each relation to its database, in the order delivered: by database
    // name, then (schema, name), so each database's lists stay in order.
    let mut attached: Vec<Attached> = Vec::new();
    let mut slot: Vec<Option<usize>> = vec![None; databases.len()];
    for (i, database) in databases.iter().enumerate().filter(|(_, d)| !d.served && !d.aside && still_attached(&d.name)) {
        slot[i] = Some(attached.len());
        attached.push(Attached {
            name: database.name.clone(),
            path: database.path.clone(),
            storage: database.storage.clone(),
            read_only: database.read_only,
            ..Attached::default()
        });
    }
    let served = |row: &Vec<serde_json::Value>| databases[database_of[&cell_str(row, 0)]].served;
    let (mut served_tables, mut served_views) = (Vec::new(), Vec::new());
    for (row, table) in table_rows.iter().zip(tables).filter(|(row, _)| present.contains(&key_of(row))) {
        match slot[database_of[&cell_str(row, 0)]] {
            Some(a) => attached[a].tables.push(table),
            None if served(row) => served_tables.push(table),
            None => {}
        }
    }
    for (row, view) in view_rows.iter().zip(views).filter(|(row, _)| present.contains(&key_of(row))) {
        match slot[database_of[&cell_str(row, 0)]] {
            Some(a) => attached[a].views.push(view),
            None if served(row) => served_views.push(view),
            None => {}
        }
    }
    attached.extend(unattached.into_iter().map(|u| Attached {
        name: u.record.name,
        path: Some(u.record.path),
        storage: u.record.storage,
        read_only: u.record.read_only,
        error: Some(u.reason),
        ..Attached::default()
    }));
    attached.sort_by(|a, b| a.name.cmp(&b.name));
    let catalog = Catalog {
        harbor_version,
        duckdb_version,
        database_size_bytes,
        wal_size_bytes,
        tables: served_tables,
        sequences,
        views: served_views,
        attached,
        stale: None,
    };
    let body = serde_json::to_string(&catalog).unwrap();
    LAST_CATALOG.lock().unwrap().0 = Some(catalog);
    BUSY.lock().unwrap().clear();
    let _ = req.respond(json_response(200, &body));
    learn_after_reading();
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

/// A client's `ATTACH` or `DETACH` of `name`, pending
/// ([`workspace::pending`]) from before it is sent until it is over, when
/// its executor tells whether it ran (`Job::claims`) and the name is
/// claimed when it did ([`workspace::settled`]). No lock is held between:
/// a catalog read passes a file set aside under a pending name by, and
/// waits for nothing. Dropped as its request ends, after the answer is
/// written: the executor tells as the statement ends, and the answer may be
/// all its executor holds.
struct Claiming {
    name: String,
    ran: mpsc::Receiver<bool>,
}

impl Claiming {
    fn new(name: &[u8], ran: mpsc::Receiver<bool>) -> Self {
        let name = String::from_utf8_lossy(name).into_owned();
        workspace::pending(&name);
        Claiming { name, ran }
    }
}

impl Drop for Claiming {
    fn drop(&mut self) {
        workspace::settled(&self.name, self.ran.recv() == Ok(true));
    }
}

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

    // A client's DETACH or ATTACH of a name that ran is the client's: a
    // file the server set aside under it, or one the server attached under
    // it from the workspace's record, is not the server's to attach again.
    // Claimed once the statement is over, when it ran, and pending until
    // then, so the server does not attach one again in between.
    let named = detached_name(&parsed.sql).or_else(|| attached_name(&parsed.sql));
    let (mut job, ready, body) = Job::new(parsed.sql, parsed.params, shape, parsed.timeout);
    let id = job.id;
    let _claiming = named.map(|name| {
        let (claims, ran) = mpsc::sync_channel(1);
        job.claims = Some(claims);
        Claiming::new(&name, ran)
    });

    // Registered before the job is sent, so a Stop pressed the instant the
    // query goes out has something to find. `Cancellable` deregisters on drop,
    // on every path below including the early returns.
    let _cancellable = match parsed.query.as_deref() {
        None => None,
        Some(name) => match Cancellable::register(name, slot, id) {
            Ok(guard) => Some(guard),
            // The job goes first, and with it what tells `Claiming` that
            // it did not run.
            Err(refusal) => {
                drop(job);
                return refuse(req, refusal);
            }
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

// ---------------------------------------------------------------------------
// The catalog gate
//
// The engine's catalog functions walk every attached database, and a DETACH
// of a SQLite database that lands during the walk brings the engine down: a
// segfault of the process, or `FATAL … database has been invalidated`, after
// which every statement fails until a restart. Measured on alpha42289, one
// client attaching and detaching a SQLite file while two read the catalog:
// duckdb_tables(), duckdb_columns(), duckdb_schemas(), information_schema,
// SHOW ALL TABLES, sql_auto_complete and /catalog each broke the server
// within ten seconds, and a plain query or duckdb_databases() never did. The
// bug is the engine's; the server keeps the two apart. A statement that
// attaches or detaches runs alone against every statement that reads the
// catalog, and those run beside each other and beside everything else.
// ---------------------------------------------------------------------------

/// Shared by each statement that reads the catalog, and held alone by each
/// that attaches or detaches, for as long as it runs ([`Share`],
/// [`Alone`]); and the signal that it was let go of.
static CATALOG_GATE: (Mutex<GateState>, Condvar) = (Mutex::new(GateState::new()), Condvar::new());

/// How often a statement that waits at the gate looks whether it was
/// cancelled: by its client, by leaving or by a deadline.
const GATE_LOOK: Duration = Duration::from_millis(10);

/// How long a statement waits for an ATTACH or DETACH that keeps new shares
/// out, before it shares the gate beside it, while what the ATTACH waits for
/// is a share it found when it began to keep them out: a long statement that
/// came first holds the ATTACH, and then the statements after it would wait
/// for as long as it runs.
const PASS_BESIDE: Duration = Duration::from_millis(500);

/// Who holds the catalog gate, and who waits for it.
struct GateState {
    /// Shares held.
    shares: usize,
    /// Held alone.
    alone: bool,
    /// The statements that wait to hold it alone and keep new shares out.
    keeping: usize,
    /// How many shares were given: the next share's number.
    given: u64,
    /// The first share given since `keeping` became more than none.
    kept_from: u64,
    /// While `keeping`, the shares held that were given before `kept_from`.
    older: usize,
    /// The shares of statements in autocommit held only because the names
    /// were not all known, with their statements, to be let go of once the
    /// names say a statement reads nothing (`let_unread_go`).
    unknown: Vec<Unknown>,
}

/// A share held only because the names were not all known.
struct Unknown {
    number: u64,
    sql: Arc<str>,
    /// [`RESHAPED`] as the statement passed the gate.
    reshaped: u64,
    /// Still counted in `GateState::shares`.
    held: bool,
}

impl GateState {
    const fn new() -> Self {
        GateState { shares: 0, alone: false, keeping: 0, given: 0, kept_from: 0, older: 0, unknown: Vec::new() }
    }

    /// A share, numbered.
    fn give(&mut self) -> u64 {
        self.shares += 1;
        self.given += 1;
        self.given - 1
    }

    /// Share `number` is let go of.
    fn let_go(&mut self, number: u64) {
        self.shares -= 1;
        if self.keeping > 0 && number < self.kept_from {
            self.older = self.older.saturating_sub(1);
        }
    }

    /// Whether a statement that has waited `waited` may share it now.
    fn shares_now(&self, waited: Duration) -> bool {
        !self.alone && (self.keeping == 0 || (self.older > 0 && waited >= PASS_BESIDE))
    }

    /// Held alone, when nothing holds it.
    fn take_alone(&mut self) -> bool {
        let free = !self.alone && self.shares == 0;
        self.alone |= free;
        free
    }

    /// A statement waiting to hold it alone keeps new shares out.
    fn keep(&mut self) {
        self.keeping += 1;
        if self.keeping == 1 {
            self.kept_from = self.given;
            self.older = self.shares;
        }
    }
}

fn gate_state() -> std::sync::MutexGuard<'static, GateState> {
    CATALOG_GATE.0.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Wait at most `look` for the gate to be let go of.
fn gate_wait(state: std::sync::MutexGuard<'static, GateState>, look: Duration) -> std::sync::MutexGuard<'static, GateState> {
    CATALOG_GATE.1.wait_timeout(state, look).unwrap_or_else(PoisonError::into_inner).0
}

/// The catalog gate shared, until dropped.
pub(crate) struct Share {
    number: u64,
}

impl Drop for Share {
    fn drop(&mut self) {
        let mut state = gate_state();
        let held = match state.unknown.iter().position(|u| u.number == self.number) {
            Some(at) => state.unknown.swap_remove(at).held,
            None => true,
        };
        if held {
            state.let_go(self.number);
        }
        drop(state);
        CATALOG_GATE.1.notify_all();
    }
}

/// The catalog gate held alone, until dropped or shared (`Alone::share`).
pub(crate) struct Alone {
    held: bool,
}

impl Alone {
    /// The gate shared in place of alone, with nothing let in between.
    fn share(mut self) -> Share {
        let mut state = gate_state();
        state.alone = false;
        self.held = false;
        let number = state.give();
        drop(state);
        CATALOG_GATE.1.notify_all();
        Share { number }
    }
}

impl Drop for Alone {
    fn drop(&mut self) {
        if self.held {
            gate_state().alone = false;
            CATALOG_GATE.1.notify_all();
        }
    }
}

/// The gate shared now, unless it is held alone or kept.
fn try_share() -> Option<Share> {
    let mut state = gate_state();
    (!state.alone && state.keeping == 0).then(|| Share { number: state.give() })
}

/// The gate shared now, unless it is held alone: for harbor's own short
/// reads that an ATTACH or DETACH that keeps new shares out waits for.
fn share_beside() -> Option<Share> {
    let mut state = gate_state();
    (!state.alone).then(|| Share { number: state.give() })
}

/// The gate held alone, by a statement on `slot` or harbor's own (`None`),
/// once every share is let go of: new shares are kept out once `keep` says
/// so, asked while they are not. None once the statement was cancelled
/// while it waits, or once `patience` has passed.
fn hold_alone(slot: Option<&SlotState>, patience: Option<Duration>, mut keep: impl FnMut() -> bool) -> Option<Alone> {
    let began = Instant::now();
    let mut keeping = false;
    let mut state = gate_state();
    loop {
        if state.take_alone() {
            state.keeping -= keeping as usize;
            return Some(Alone { held: true });
        }
        if slot.is_some_and(SlotState::cancelled) || patience.is_some_and(|p| began.elapsed() >= p) {
            state.keeping -= keeping as usize;
            drop(state);
            CATALOG_GATE.1.notify_all();
            return None;
        }
        if !keeping {
            drop(state);
            keeping = keep();
            state = gate_state();
            if keeping {
                state.keep();
            }
            if !state.alone && state.shares == 0 {
                continue;
            }
        }
        state = gate_wait(state, GATE_LOOK);
    }
}

/// The gate shared, by a statement on `slot` or harbor's own (`None`), as
/// a share in `unknown` when given one: once nothing holds it alone, and
/// nothing keeps new shares out, or for [`PASS_BESIDE`] while an ATTACH
/// that does waits for a share it found. None once the statement was
/// cancelled while it waits.
fn wait_share(slot: Option<&SlotState>, unknown: Option<(Arc<str>, u64)>) -> Option<Share> {
    let began = Instant::now();
    let mut state = gate_state();
    loop {
        if state.shares_now(began.elapsed()) {
            let number = state.give();
            if let Some((sql, reshaped)) = unknown {
                state.unknown.push(Unknown { number, sql, reshaped, held: true });
            }
            return Some(Share { number });
        }
        if slot.is_some_and(SlotState::cancelled) {
            return None;
        }
        state = gate_wait(state, GATE_LOOK);
    }
}

/// What a statement is to the catalog gate.
#[derive(Debug, PartialEq, Eq)]
enum Gate {
    /// Neither attaches nor reads the catalog.
    Open,
    /// Reads the catalog: shares the gate.
    Reads,
    /// Attaches or detaches a database: holds the gate alone.
    Changes,
}

/// The gate a statement passes, from its text. `ATTACH` and `DETACH` are
/// read as the keyword the engine acts on, through comments and an analyzed
/// `EXPLAIN`. A statement reads the catalog when it is a `SHOW`, `DESCRIBE`,
/// `SUMMARIZE` or `PRAGMA`, or names, in its code or as a quoted name, a
/// catalog function or schema: `duckdb_…`, `pragma_…`, `pg_…`,
/// `information_schema`, `sql_auto_complete` or `sqlite_query`, which reads
/// an attached SQLite database by its name; `query` or `query_table`
/// called, which run a statement or read a table named in a string; or a
/// view, macro or prepared statement whose body reads it (`CatalogNames`).
/// A name in a string or a comment does not count, and neither does a
/// column named `query`. While the views and macros are not all learned
/// (`CatalogNames::complete`), every statement that names anything
/// (`names_any`) reads the catalog, since any name may be one that does.
fn catalog_gate(sql: &str) -> Gate {
    gate_of(sql, &CATALOG_NAMES.read().unwrap_or_else(PoisonError::into_inner))
}

/// The gate a statement passes, and whether it reads the catalog only
/// because the names are not all known.
fn catalog_gate_known(sql: &str) -> (Gate, bool) {
    let names = CATALOG_NAMES.read().unwrap_or_else(PoisonError::into_inner);
    let gate = gate_of(sql, &names);
    let unknown = gate == Gate::Reads && !names.complete && gate_in(sql, &names, true) == Gate::Open;
    (gate, unknown)
}

fn gate_of(sql: &str, names: &CatalogNames) -> Gate {
    gate_in(sql, names, names.complete)
}

/// The gate as the names say, as if all known when `complete`.
fn gate_in(sql: &str, names: &CatalogNames, complete: bool) -> Gate {
    match acting_keyword(sql).as_str() {
        "ATTACH" | "DETACH" => return Gate::Changes,
        "SHOW" | "DESCRIBE" | "SUMMARIZE" | "PRAGMA" => return Gate::Reads,
        _ => {}
    }
    match names_in(sql, |name| catalog_word(name) || names.reads.contains(name)) {
        true => Gate::Reads,
        false if !complete && names_any(sql) => Gate::Reads,
        false => Gate::Open,
    }
}

/// The engine's reserved keywords (`duckdb_keywords()`). One is a name only
/// quoted, or as the part after a `.` of a name whose first part is not
/// one.
const RESERVED: &[&[u8]] = &[
    b"all", b"analyse", b"analyze", b"and", b"any", b"array", b"as", b"asc", b"asymmetric", b"both", b"case",
    b"cast", b"check", b"collate", b"column", b"constraint", b"create", b"default", b"deferrable", b"desc",
    b"describe", b"distinct", b"do", b"else", b"end", b"except", b"false", b"fetch", b"for", b"foreign", b"from",
    b"group", b"having", b"in", b"initially", b"intersect", b"into", b"lambda", b"lateral", b"leading", b"limit",
    b"not", b"null", b"offset", b"on", b"only", b"or", b"order", b"pivot", b"pivot_longer", b"pivot_wider",
    b"placing", b"primary", b"qualify", b"references", b"returning", b"select", b"show", b"some", b"summarize",
    b"symmetric", b"table", b"then", b"to", b"trailing", b"true", b"union", b"unique", b"unpivot", b"using",
    b"variadic", b"when", b"where", b"window", b"with",
];

/// Whether a statement names anything a view, a macro or a catalog
/// function may be: a quoted name, or a word of its code that is neither
/// a reserved keyword nor a number, digits and `_` after a digit.
/// `SELECT 42` names nothing.
fn names_any(sql: &str) -> bool {
    let plain = wire::scan::plain_spaces(sql.as_bytes());
    let bytes = &*plain;
    let is_word = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80;
    wire::scan::spans(bytes).iter().any(|span| {
        let text = &bytes[span.start..span.end];
        match span.kind {
            wire::scan::Kind::Code => text.split(|&c| !is_word(c)).filter(|word| !word.is_empty()).any(|word| {
                let number = word[0].is_ascii_digit() && word.iter().all(|&c| c.is_ascii_digit() || c == b'_');
                !number && !RESERVED.contains(&word.to_ascii_lowercase().as_slice())
            }),
            wire::scan::Kind::Str => matches!(text.first(), Some(b'"' | b'`')),
            _ => false,
        }
    })
}

/// Whether a name, lowercased, is one of the engine's catalog functions or
/// schemas, as `names_in` passes it: `query` and `query_table` count
/// called, as `query(` and `query_table(`, and a column of either name does
/// not.
fn catalog_word(word: &[u8]) -> bool {
    ["duckdb_", "pragma_", "pg_"].iter().any(|p| word.starts_with(p.as_bytes()))
        || word == b"information_schema"
        || word == b"sql_auto_complete"
        || word == b"sqlite_query"
        || word == b"query("
        || word == b"query_table("
}

/// The functions that read the catalog only when called, and whose names a
/// column or a table may have.
const CALLED: [&[u8]; 2] = [b"query", b"query_table"];

/// Each name a statement uses, lowercased, as the engine matches names: a
/// word of its code, or a name quoted in `"` or `` ` ``, until `found` says
/// it is the one looked for. Whether one was. Its spaces are the engine's
/// (`wire::scan::plain_spaces`), and one of the `CALLED` names followed by
/// `(`, past spaces and comments, is passed with it: `query(`.
fn names_in(sql: &str, mut found: impl FnMut(&[u8]) -> bool) -> bool {
    let is_word = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80;
    let plain = wire::scan::plain_spaces(sql.as_bytes());
    let bytes = &*plain;
    let spans = wire::scan::spans(bytes);
    let mut used = |name: &[u8], next: &[wire::scan::Span], from: usize| {
        let mut name = name.to_ascii_lowercase();
        if CALLED.contains(&name.as_slice()) && called(bytes, next, from) {
            name.push(b'(');
        }
        found(&name)
    };
    for (at, span) in spans.iter().enumerate() {
        let text = &bytes[span.start..span.end];
        match span.kind {
            wire::scan::Kind::Code => {
                let mut i = span.start;
                while i < span.end {
                    if !is_word(bytes[i]) {
                        i += 1;
                        continue;
                    }
                    let start = i;
                    while i < span.end && is_word(bytes[i]) {
                        i += 1;
                    }
                    if used(&bytes[start..i], &spans[at..], i) {
                        return true;
                    }
                }
            }
            wire::scan::Kind::Str if matches!(text.first(), Some(b'"' | b'`')) => {
                let quote = text[0];
                let name = text[1..].strip_suffix(&[quote]).unwrap_or(&text[1..]);
                if used(name, &spans[at + 1..], span.end) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Whether the code that comes next in `spans` from `from`, past spaces and
/// comments, is `(`.
fn called(bytes: &[u8], spans: &[wire::scan::Span], from: usize) -> bool {
    for span in spans {
        match span.kind {
            wire::scan::Kind::Code => {
                if let Some(&c) = bytes[from.max(span.start)..span.end].iter().find(|&&c| !wire::scan::is_space(c)) {
                    return c == b'(';
                }
            }
            wire::scan::Kind::LineComment | wire::scan::Kind::BlockComment => {}
            _ => return false,
        }
    }
    false
}

/// The views and macros whose bodies read the catalog. A view over
/// `duckdb_tables()` reads it when it is read, and `SELECT * FROM v` names
/// no catalog function, so the gate knows such names. Each view's and
/// macro's name is kept with the names its body uses, learned from the
/// engine's catalog when the pool opens and after each `ATTACH`, `IMPORT
/// DATABASE` and `COPY FROM DATABASE`, since each brings views of its own,
/// and from each `CREATE VIEW`, `CREATE MACRO`, `PREPARE` and `ALTER VIEW …
/// RENAME` as it passes, before it runs, and the views and macros of the
/// `schema.sql` an `IMPORT DATABASE` runs, before it runs. A name is never
/// forgotten: one dropped, or kept by another schema's view of its name,
/// costs a statement that names it the shared gate and nothing else.
///
/// The engine's listing reads every attached database's catalog, and one
/// SQLite file locked fails it after the file's wait. Until a listing
/// succeeds, and from before a statement that brings views runs until
/// they are read, the names are not all known (`complete`), and every
/// statement shares the gate: an ATTACH or DETACH waits for the statements
/// running, and nothing reads the catalog unseen. Learning is tried again
/// after each ATTACH and DETACH (`learn_again`) and each `/catalog` read
/// that succeeds (`learn_after_reading`).
#[derive(Default)]
struct CatalogNames {
    /// Each name, lowercased, with the names its bodies use.
    uses: HashMap<Vec<u8>, std::collections::HashSet<Vec<u8>>>,
    /// Each name used, with the names whose bodies use it.
    used_by: HashMap<Vec<u8>, std::collections::HashSet<Vec<u8>>>,
    /// The names whose bodies read the catalog, themselves or through
    /// another such name.
    reads: std::collections::HashSet<Vec<u8>>,
    /// Every view and macro the engine lists was learned by the last
    /// listing, and nothing since brought one unread.
    complete: bool,
    /// How many times the names were found incomplete (`unsettle`), so a
    /// listing read before the last time cannot call them complete.
    unsettled: u64,
}

static CATALOG_NAMES: std::sync::LazyLock<RwLock<CatalogNames>> = std::sync::LazyLock::new(Default::default);

/// The names `body` uses, lowercased.
fn used_names(body: &str) -> std::collections::HashSet<Vec<u8>> {
    let mut used = std::collections::HashSet::new();
    names_in(body, |word| {
        used.insert(word.to_vec());
        false
    });
    used
}

impl CatalogNames {
    /// Names whose listing succeeded, holding none yet.
    #[cfg(test)]
    fn listed() -> Self {
        CatalogNames { complete: true, ..Default::default() }
    }

    /// `name`, whose body is `body`.
    #[cfg(test)]
    fn learn(&mut self, name: &[u8], body: &str) {
        self.learn_all(vec![(name.to_vec(), used_names(body))]);
    }

    /// Not all learned: a listing failed, or a statement brings views.
    fn unsettle(&mut self) {
        self.complete = false;
        self.unsettled += 1;
    }

    /// Each name with the names its body uses ([`used_names`]), then what
    /// reads the catalog among them and the names that use them, settled
    /// once: as cheap for the whole catalog as for one name.
    fn learn_all(&mut self, learned: Vec<(Vec<u8>, std::collections::HashSet<Vec<u8>>)>) {
        let mut names = Vec::with_capacity(learned.len());
        for (name, used) in learned {
            let name = name.to_ascii_lowercase();
            self.add(&name, used);
            names.push(name);
        }
        self.settle(names);
    }

    /// `to` takes what `from` uses, as a view renamed does.
    fn rename(&mut self, from: &[u8], to: &[u8]) {
        let used = self.uses.get(from).cloned().unwrap_or_default();
        self.add(to, used);
        self.settle(vec![to.to_vec()]);
    }

    fn add(&mut self, name: &[u8], used: std::collections::HashSet<Vec<u8>>) {
        for u in &used {
            self.used_by.entry(u.clone()).or_default().insert(name.to_vec());
        }
        self.uses.entry(name.to_vec()).or_default().extend(used);
    }

    /// Mark each of `names` that reads the catalog, and every name that
    /// uses one marked, through the names that use it: each name is marked
    /// once, so the work is the names and their uses, once.
    fn settle(&mut self, names: Vec<Vec<u8>>) {
        let reads = |this: &Self, name: &Vec<u8>| {
            this.uses.get(name).is_some_and(|used| used.iter().any(|u| catalog_word(u) || this.reads.contains(u)))
        };
        let mut work: Vec<Vec<u8>> = names.into_iter().filter(|n| !self.reads.contains(n) && reads(self, n)).collect();
        while let Some(name) = work.pop() {
            if !self.reads.insert(name.clone()) {
                continue;
            }
            if let Some(users) = self.used_by.get(&name) {
                work.extend(users.iter().filter(|u| !self.reads.contains(*u)).cloned());
            }
        }
    }
}

/// Learn the views and macros a statement makes or renames, before it runs
/// (`CatalogNames`), and those of the `schema.sql` an `IMPORT DATABASE`
/// runs.
fn learn_from_statement(sql: &str) {
    match acting_keyword(sql).as_str() {
        "CREATE" | "ALTER" | "PREPARE" => learn_made(sql),
        "IMPORT" => {
            for statement in imported_statements(sql) {
                if matches!(acting_keyword(&statement).as_str(), "CREATE" | "ALTER") {
                    learn_made(&statement);
                }
            }
        }
        _ => {}
    }
}

fn learn_made(sql: &str) {
    let Some(made) = made_name(sql) else { return };
    let used = match &made {
        Made::Body(_) => used_names(sql),
        Made::Renamed { .. } => Default::default(),
    };
    let mut names = CATALOG_NAMES.write().unwrap_or_else(PoisonError::into_inner);
    match made {
        Made::Body(name) => names.learn_all(vec![(name, used)]),
        Made::Renamed { from, to } => names.rename(&from, &to),
    }
}

/// The statements of the `schema.sql` an `IMPORT DATABASE 'directory'`
/// runs, where the engine finds it: none when there is no such file.
fn imported_statements(sql: &str) -> Vec<String> {
    let bytes = sql.as_bytes();
    let Some(path) = wire::scan::scan(sql)
        .into_iter()
        .find(|s| matches!(s.kind, wire::scan::Kind::Str) && bytes[s.start] == b'\'')
    else {
        return Vec::new();
    };
    let quoted = &sql[path.start + 1..path.end.saturating_sub(1).max(path.start + 1)];
    let directory = quoted.replace("''", "'");
    let Ok(schema) = std::fs::read_to_string(std::path::Path::new(&directory).join("schema.sql")) else {
        return Vec::new();
    };
    statements_of(&schema)
}

/// `script`'s statements, split at each `;` of its code.
fn statements_of(script: &str) -> Vec<String> {
    let bytes = script.as_bytes();
    let mut statements = Vec::new();
    let mut start = 0;
    for span in wire::scan::scan(script).into_iter().filter(|s| matches!(s.kind, wire::scan::Kind::Code)) {
        for at in (span.start..span.end).filter(|&i| bytes[i] == b';') {
            statements.push(script[start..at].to_string());
            start = at + 1;
        }
    }
    statements.push(script[start..].to_string());
    statements.retain(|s| !s.trim().is_empty());
    statements
}

/// What lists every view and macro with its body, one JSON pair a row.
const LISTING: &str = "SELECT json_array(view_name, sql)::VARCHAR FROM duckdb_views() WHERE sql IS NOT NULL \
                       UNION ALL SELECT json_array(function_name, macro_definition)::VARCHAR FROM duckdb_functions() \
                       WHERE function_type IN ('macro', 'table_macro') AND macro_definition IS NOT NULL";

/// Learn every view and macro the engine lists on `conn` (`CatalogNames`).
/// Its catalog is read, so the caller holds the gate. Said in the log when
/// the catalog does not read; what was learned stays, and the names are
/// not complete until a listing succeeds.
fn learn_catalog_names(conn: &mut Connection) {
    let began = CATALOG_NAMES.read().unwrap_or_else(PoisonError::into_inner).unsettled;
    let reshaped = RESHAPED.load(Ordering::SeqCst);
    let listed = conn.query_strings(LISTING).map_err(|e| e.into_text());
    learn_listed(began, reshaped, listed);
}

/// Whether the last listing failed, so the next that succeeds says so.
static UNLISTED: AtomicBool = AtomicBool::new(false);

/// Learn the rows of a listing begun when the names had been unsettled
/// `began` times, and [`RESHAPED`] was `reshaped`: complete when it
/// succeeded and nothing unsettled them since, and then the statements it
/// says read nothing are let go of (`let_unread_go`).
fn learn_listed(began: u64, reshaped: u64, listed: Result<Vec<String>, String>) {
    let rows = match listed {
        Ok(rows) => rows,
        Err(e) => {
            if !UNLISTED.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "harbor: the views and macros that read the catalog were not read from it, and until they \
                     are, an ATTACH or DETACH waits for every statement running: {e}"
                );
            }
            CATALOG_NAMES.write().unwrap_or_else(PoisonError::into_inner).unsettle();
            return;
        }
    };
    // Read outside the lock, which the gate of every statement takes.
    let learned: Vec<_> = rows
        .iter()
        .filter_map(|row| serde_json::from_str::<(String, String)>(row).ok())
        .map(|(name, body)| (name.into_bytes(), used_names(&body)))
        .collect();
    let mut names = CATALOG_NAMES.write().unwrap_or_else(PoisonError::into_inner);
    names.learn_all(learned);
    if names.unsettled == began {
        names.complete = true;
        UNREAD_BESIDE.store(false, Ordering::SeqCst);
        if UNLISTED.swap(false, Ordering::Relaxed) {
            eprintln!("harbor: the views and macros that read the catalog are read from it");
        }
        let_unread_go(&names, reshaped);
    }
}

/// How many times a statement that may drop, replace or rename a view or
/// a macro (`reshapes`) began or ended, so a listing knows whether the
/// views a statement running reads may be ones it did not see.
static RESHAPED: AtomicU64 = AtomicU64::new(0);

/// How many such statements have begun and not ended.
static RESHAPING: AtomicUsize = AtomicUsize::new(0);

/// A statement that may drop, replace or rename a view or a macro, counted
/// from when it has passed the gate until it has ended.
struct Reshaping;

impl Reshaping {
    fn begin() -> Self {
        RESHAPING.fetch_add(1, Ordering::SeqCst);
        RESHAPED.fetch_add(1, Ordering::SeqCst);
        Reshaping
    }
}

impl Drop for Reshaping {
    fn drop(&mut self) {
        RESHAPED.fetch_add(1, Ordering::SeqCst);
        RESHAPING.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Whether a statement may drop, replace or rename a view or a macro, or
/// make visible a transaction's that did: every statement but those that
/// read or write rows, begin a transaction, or set a setting.
fn reshapes(sql: &str) -> bool {
    !matches!(
        acting_keyword(sql).as_str(),
        "SELECT" | "WITH" | "VALUES" | "FROM" | "TABLE" | "INSERT" | "UPDATE" | "DELETE" | "BEGIN" | "START"
            | "SHOW" | "DESCRIBE" | "SUMMARIZE" | "PIVOT" | "UNPIVOT" | "EXPLAIN" | "CHECKPOINT" | "ANALYZE"
            | "VACUUM" | "SET" | "RESET" | "USE"
    )
}

/// Let go of the shares held only because the names were not all known,
/// by statements in autocommit the names, now complete by a listing begun
/// when [`RESHAPED`] was `reshaped`, say read nothing: a statement that
/// passes the gate open now. Only those that passed the gate when it was
/// `reshaped`, and while nothing has reshaped since nor is reshaping: each
/// began its transaction after the last view it may read was made, and the
/// listing read every one of them.
fn let_unread_go(names: &CatalogNames, reshaped: u64) {
    if RESHAPING.load(Ordering::SeqCst) != 0 || RESHAPED.load(Ordering::SeqCst) != reshaped {
        return;
    }
    let mut state = gate_state();
    let mut let_go = Vec::new();
    for u in state.unknown.iter_mut().filter(|u| u.held && u.reshaped == reshaped) {
        if gate_of(&u.sql, names) == Gate::Open {
            u.held = false;
            let_go.push(u.number);
        }
    }
    for number in &let_go {
        state.let_go(*number);
    }
    drop(state);
    if !let_go.is_empty() {
        CATALOG_GATE.1.notify_all();
    }
}

/// The connection the views and macros are learned on after the pool
/// opens: one of its own, outside every session's transaction.
static LEARNER: Mutex<Option<Learner>> = Mutex::new(None);

struct Learner {
    conn: Connection,
    /// When a probe last found a SQLite file busy.
    busy_at: Option<Instant>,
    /// The attached SQLite files' probes, by path: hidden attachments of
    /// their own, which the engine's listing does not read, kept while the
    /// names are not all known, so a probe asks whether a file reads beside
    /// the statements running, with the gate shared, and is attached and
    /// detached only with it held alone (`busy_sqlite`, `let_probes_go`).
    probes: Vec<(String, String)>,
}

impl Learner {
    fn new(conn: Connection) -> Self {
        Learner { conn, busy_at: None, probes: Vec::new() }
    }

    fn asked_lately(&self) -> bool {
        self.busy_at.is_some_and(|at| at.elapsed() < ASK_AGAIN)
    }

    /// Say a file found busy, as `why`, once until the names are read.
    fn found_busy(&mut self, why: &str) {
        self.busy_at = Some(Instant::now());
        if !UNLISTED.swap(true, Ordering::Relaxed) {
            eprintln!(
                "harbor: the views and macros that read the catalog are not read while {why}, and until they \
                 are, an ATTACH or DETACH waits for every statement running"
            );
        }
    }

    /// Detach every probe, under the gate held alone.
    fn let_probes_go(&mut self) {
        for (_, probe) in std::mem::take(&mut self.probes) {
            let _ = self.conn.execute_batch(&format!("DETACH {}", wire::open::quote_ident(&probe)));
        }
    }
}

/// How long `learn_again` waits after it found a SQLite file busy before it
/// asks again.
const ASK_AGAIN: Duration = Duration::from_secs(1);

/// What a probe of a SQLite file is attached as, before a number: hidden
/// from the catalog functions, and no client's.
const PROBE: &str = "harbor_probe_";

/// Learn again, when the names are not complete, after an ATTACH or a
/// DETACH, holding the gate alone as `alone`. The listing waits out a
/// SQLite file's lock, five seconds by default, so each attached SQLite
/// file is asked first, waiting [`PROBE_BUSY_MS`] on a lock (`busy_sqlite`),
/// and while one does not read, learning waits for a later chance, and asks
/// again no sooner than [`ASK_AGAIN`], so a client attaching and detaching
/// over and over does not hold the gate for the asking. Then the gate is
/// shared for the listing: statements run beside it, and only an ATTACH or
/// DETACH waits. When the names are complete, the probes kept are let go.
pub(crate) fn learn_again(alone: Alone) {
    {
        let mut learner = LEARNER.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(learner) = learner.as_mut() else { return };
        if names_complete() {
            learner.let_probes_go();
            return;
        }
        if learner.asked_lately() {
            return;
        }
        learner.busy_at = None;
        match busy_sqlite(learner, true) {
            Ok(None) => UNREAD_BESIDE.store(false, Ordering::SeqCst),
            Ok(Some(why)) | Err(why) => return learner.found_busy(&why),
        }
    }
    // The learner is let go first: a statement holding the gate shared may
    // wait for it. The gate is shared in place of alone, so the listing
    // waits for nothing. A statement that would share the gate only because
    // the names are not all known waits for the listing (`LEARNING`), and
    // then for what it learned, rather than hold the gate for as long as it
    // runs.
    let _learning = Learning::begin();
    let _reading = alone.share();
    learn_on_learner();
}

/// Learn again beside the statements running, sharing the gate even
/// beside an ATTACH or DETACH that keeps new shares out, which waits for
/// what this learns: when every attached SQLite file has a probe kept, and
/// each reads. Whether it asked; it did not when a file has no probe.
fn learn_on_probes() -> bool {
    let Some(_reading) = share_beside() else { return false };
    let mut learner = LEARNER.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(learner) = learner.as_mut() else { return true };
    if names_complete() || learner.asked_lately() {
        return true;
    }
    match busy_sqlite(learner, false) {
        Ok(None) => UNREAD_BESIDE.store(false, Ordering::SeqCst),
        Ok(Some(why)) => {
            learner.found_busy(&why);
            return true;
        }
        Err(_) => return false,
    }
    let _learning = Learning::begin();
    learn_catalog_names(&mut learner.conn);
    true
}

/// How often harbor asks again on its own while the names are not all
/// known (`learn_while_unknown`), at first and while a SQLite file does not
/// read, and the longest it waits between asking after listings that
/// failed.
const LEARN_EVERY: Duration = Duration::from_secs(1);
const LEARN_LONGEST: Duration = Duration::from_secs(30);

/// How long harbor's own asking keeps new shares of the gate out while it
/// waits for the gate alone, before it gives up for the turn.
const LEARN_PATIENCE: Duration = Duration::from_millis(20);

/// Learn again on harbor's own, until `stop`, each [`LEARN_EVERY`] while
/// the names are not all known, so they are known again soon after a
/// SQLite file that did not read reads again, without waiting for a
/// client's ATTACH, DETACH or `/catalog`: on the probes kept, beside the
/// statements running (`learn_on_probes`), or as `learn_again` asks, once
/// the gate is free for it within [`LEARN_PATIENCE`]. After a listing that
/// failed, asked half as often each time, up to [`LEARN_LONGEST`]. Once the
/// names are complete, the probes kept are let go when the gate is free.
fn learn_while_unknown(stop: Arc<AtomicBool>) {
    let mut every = LEARN_EVERY;
    let mut asked = Instant::now();
    while !stop.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(100));
        if asked.elapsed() < every {
            continue;
        }
        asked = Instant::now();
        if names_complete() {
            every = LEARN_EVERY;
            let kept = LEARNER.lock().unwrap_or_else(PoisonError::into_inner).as_ref().is_some_and(|l| !l.probes.is_empty());
            if kept && let Some(alone) = hold_alone(None, Some(LEARN_PATIENCE), || true) {
                learn_again(alone);
            }
            continue;
        }
        if !learn_on_probes() {
            let Some(alone) = hold_alone(None, Some(LEARN_PATIENCE), || true) else { continue };
            learn_again(alone);
        }
        let busy = LEARNER.lock().unwrap_or_else(PoisonError::into_inner).as_ref().is_some_and(Learner::asked_lately);
        every = match names_complete() || busy {
            true => LEARN_EVERY,
            false => (every * 2).min(LEARN_LONGEST),
        };
    }
}

/// How long an ATTACH or DETACH that waits while the names are not all
/// known lets readers in beside it and asks for the names, before it waits
/// for the readers running and keeps out the ones that come after, unless
/// asking may yet learn them.
const ASK_BESIDE: Duration = Duration::from_secs(1);

/// How many listings `learn_again` has begun and not ended.
static LEARNING: AtomicUsize = AtomicUsize::new(0);

/// A listing begun by `learn_again`, counted in [`LEARNING`] until it is
/// learned, or failed.
struct Learning;

impl Learning {
    fn begin() -> Self {
        LEARNING.fetch_add(1, Ordering::SeqCst);
        Learning
    }
}

impl Drop for Learning {
    fn drop(&mut self) {
        LEARNING.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Whether every view and macro is known (`CatalogNames::complete`).
fn names_complete() -> bool {
    CATALOG_NAMES.read().unwrap_or_else(PoisonError::into_inner).complete
}

/// Learn again, when the names are not complete, for an ATTACH or DETACH
/// that waits for the statements running, sharing the gate beside them: a
/// listing that succeeds makes the names complete, and the statements that
/// come after it, the catalog not read, pass the gate by rather than wait
/// for the ATTACH. Asked no more often than [`ASK_AGAIN`] after a listing or
/// a probe found a SQLite file busy, never while an ATTACH or DETACH waits
/// or another listing runs, and not again after a listing here failed until
/// a probe finds every SQLite file reads (`UNREAD_BESIDE`): a listing waits
/// out a locked file, and the ATTACH with it. Whether asking again may yet
/// learn them.
fn learn_beside() -> bool {
    if UNREAD_BESIDE.load(Ordering::SeqCst) {
        return false;
    }
    let Some(_reading) = try_share() else { return true };
    let Ok(mut learner) = LEARNER.try_lock() else { return true };
    let Some(learner) = learner.as_mut() else { return false };
    if names_complete() {
        return true;
    }
    if learner.asked_lately() {
        return false;
    }
    learn_catalog_names(&mut learner.conn);
    if !names_complete() {
        learner.busy_at = Some(Instant::now());
        UNREAD_BESIDE.store(true, Ordering::SeqCst);
        return false;
    }
    true
}

/// Whether a listing beside the statements running failed, and neither a
/// probe has found every SQLite file reads since nor a listing read.
static UNREAD_BESIDE: AtomicBool = AtomicBool::new(false);

/// Learn again, when the names are not complete, after a `/catalog` read
/// that succeeded, which read every attached database's catalog.
fn learn_after_reading() {
    if CATALOG_NAMES.read().unwrap_or_else(PoisonError::into_inner).complete {
        return;
    }
    let _reading = reading_catalog();
    learn_on_learner();
}

/// Learn on [`LEARNER`], under the gate the caller holds.
fn learn_on_learner() {
    let mut learner = LEARNER.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(learner) = learner.as_mut() {
        learn_catalog_names(&mut learner.conn);
    }
}

/// An attached SQLite file that does not read for the moment, and why: each
/// asked as `reads_alone` asks it, on a probe of its own that waits
/// [`PROBE_BUSY_MS`] on a lock, kept in `learner.probes`. With the gate
/// held alone (`alone`), a file without a probe has one attached, and a
/// probe whose file is no longer attached is detached; with it shared, a
/// file without a probe is an `Err`.
fn busy_sqlite(learner: &mut Learner, alone: bool) -> Result<Option<String>, String> {
    let conn = &mut learner.conn;
    let paths = match conn.query_strings(
        "SELECT path FROM duckdb_databases() WHERE type = 'sqlite' AND path IS NOT NULL AND path NOT IN ('', ':memory:')",
    ) {
        Ok(paths) => paths,
        Err(e) => return Ok(Some(format!("the attached databases do not list: {}", e.into_text()))),
    };
    let quote = wire::open::quote_literal;
    if alone {
        let (kept, gone): (Vec<_>, Vec<_>) = std::mem::take(&mut learner.probes).into_iter().partition(|(p, _)| paths.contains(p));
        for (_, probe) in gone {
            let _ = conn.execute_batch(&format!("DETACH {}", wire::open::quote_ident(&probe)));
        }
        learner.probes = kept;
    }
    let why = |e: crate::engine::Error| e.into_text().split("\nLINE ").next().unwrap_or_default().trim().to_string();
    for path in &paths {
        let probe = match learner.probes.iter().find(|(p, _)| p == path) {
            Some((_, probe)) => probe.clone(),
            None if !alone => return Err(format!("{path} has no probe")),
            None => {
                let probe = format!("{PROBE}{}", next_job_id());
                let name = wire::open::quote_ident(&probe);
                let attach = format!("ATTACH {} AS {name} (TYPE sqlite, READ_ONLY, BUSY_TIMEOUT {PROBE_BUSY_MS}, HIDDEN)", quote(path));
                if let Err(e) = conn.execute_batch(&attach) {
                    return Ok(Some(format!("{path} does not read ({})", why(e))));
                }
                learner.probes.push((path.clone(), probe.clone()));
                probe
            }
        };
        let read = conn.query_strings(&format!(
            "SELECT count(*)::VARCHAR FROM sqlite_query({}, 'SELECT count(*) FROM sqlite_master')",
            quote(&probe)
        ));
        if let Err(e) = read {
            return Ok(Some(format!("{path} does not read ({})", why(e))));
        }
    }
    Ok(None)
}

/// Whether a statement brings views and macros the engine makes, read from
/// the engine once it has run: an `ATTACH` but of a SQLite file
/// (`attaches_sqlite`), whose views are SQLite's, so that reading its
/// catalog, which waits on its file, is spared, and so is harbor's own
/// question whether one reads; an `IMPORT DATABASE`; and a `COPY FROM
/// DATABASE`.
fn brings_views(sql: &str) -> bool {
    match acting_keyword(sql).as_str() {
        "ATTACH" => !attaches_sqlite(sql),
        "IMPORT" => true,
        "COPY" => {
            let mut tokens = Vec::new();
            names_in_order(sql, &mut tokens);
            let word = |t: Option<&Token>, w: &str| matches!(t, Some(Token::Word(t)) if t == w.as_bytes());
            tokens
                .iter()
                .position(|t| word(Some(t), "copy"))
                .is_some_and(|at| word(tokens.get(at + 1), "from") && word(tokens.get(at + 2), "database"))
        }
        _ => false,
    }
}

/// Whether an `ATTACH` attaches a SQLite file, as the engine reads the
/// statement: a path led by `sqlite:`, or a `TYPE sqlite` option. A
/// database named `sqlite` is not one.
fn attaches_sqlite(sql: &str) -> bool {
    let sqlite = |name: &[u8]| name == b"sqlite" || name == b"sqlite3";
    let is_word = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80;
    let plain = wire::scan::plain_spaces(sql.as_bytes());
    let bytes = &*plain;
    let (mut path, mut after_type) = (true, false);
    for span in wire::scan::spans(bytes) {
        let text = &bytes[span.start..span.end];
        match span.kind {
            wire::scan::Kind::Code => {
                for word in text.split(|&c| !is_word(c)).filter(|w| !w.is_empty()) {
                    let word = word.to_ascii_lowercase();
                    if after_type && sqlite(&word) {
                        return true;
                    }
                    after_type = word == b"type";
                }
            }
            wire::scan::Kind::Str => {
                let inner = text.get(1..text.len().saturating_sub(1)).unwrap_or_default().to_ascii_lowercase();
                if path && text[0] == b'\'' {
                    path = false;
                    if inner.starts_with(b"sqlite:") || inner.starts_with(b"sqlite3:") {
                        return true;
                    }
                } else if after_type && sqlite(&inner) {
                    return true;
                }
                after_type = false;
            }
            wire::scan::Kind::LineComment | wire::scan::Kind::BlockComment => {}
            wire::scan::Kind::Dollar => after_type = false,
        }
    }
    false
}

/// What a `CREATE` or `ALTER` makes, by its last name part, lowercased.
#[derive(Debug, PartialEq, Eq)]
enum Made {
    /// A view or a macro, whose body is the statement.
    Body(Vec<u8>),
    /// A view renamed.
    Renamed { from: Vec<u8>, to: Vec<u8> },
}

/// The view or macro a statement makes: `CREATE [OR REPLACE] [TEMP |
/// TEMPORARY] [RECURSIVE] VIEW | MACRO | FUNCTION [IF NOT EXISTS] name`,
/// or `ALTER VIEW [IF EXISTS] name RENAME TO name`, a name being one to
/// three parts, each a word or a quoted name; or the prepared statement a
/// `PREPARE name AS` makes, which `EXECUTE name` runs.
fn made_name(sql: &str) -> Option<Made> {
    let mut tokens = Vec::new();
    names_in_order(sql, &mut tokens);
    let mut at = 0;
    let word = |at: usize, w: &str| matches!(tokens.get(at), Some(Token::Word(t)) if t == w.as_bytes());
    let name = |at: &mut usize| -> Option<Vec<u8>> {
        let mut last = None;
        loop {
            match tokens.get(*at) {
                Some(Token::Word(n) | Token::Quoted(n)) => last = Some(n.clone()),
                _ => return last,
            }
            *at += 1;
            if !matches!(tokens.get(*at), Some(Token::Dot)) {
                return last;
            }
            *at += 1;
        }
    };
    if word(at, "create") {
        at += 1;
        if word(at, "or") && word(at + 1, "replace") {
            at += 2;
        }
        if word(at, "temp") || word(at, "temporary") {
            at += 1;
        }
        if word(at, "recursive") {
            at += 1;
        }
        if !(word(at, "view") || word(at, "macro") || word(at, "function")) {
            return None;
        }
        at += 1;
        if word(at, "if") && word(at + 1, "not") && word(at + 2, "exists") {
            at += 3;
        }
        return name(&mut at).map(Made::Body);
    }
    if word(at, "prepare") {
        at += 1;
        return name(&mut at).map(Made::Body);
    }
    if word(at, "alter") && word(at + 1, "view") {
        at += 2;
        if word(at, "if") && word(at + 1, "exists") {
            at += 2;
        }
        let from = name(&mut at)?;
        if !(word(at, "rename") && word(at + 1, "to")) {
            return None;
        }
        at += 2;
        let to = name(&mut at)?;
        return Some(Made::Renamed { from, to });
    }
    None
}

/// The database a `DETACH [DATABASE] [IF EXISTS] name` names, lowercased.
fn detached_name(sql: &str) -> Option<Vec<u8>> {
    if acting_keyword(sql) != "DETACH" {
        return None;
    }
    let mut tokens = Vec::new();
    names_in_order(sql, &mut tokens);
    let word = |at: usize, w: &str| matches!(tokens.get(at), Some(Token::Word(t)) if t == w.as_bytes());
    let mut at = 1;
    if word(at, "database") {
        at += 1;
    }
    if word(at, "if") && word(at + 1, "exists") {
        at += 2;
    }
    match tokens.get(at) {
        Some(Token::Word(name) | Token::Quoted(name)) => Some(name.clone()),
        _ => None,
    }
}

/// The database an `ATTACH` names, lowercased: the name after `AS`, else
/// the file's name without its extension, as the engine names it.
fn attached_name(sql: &str) -> Option<Vec<u8>> {
    if acting_keyword(sql) != "ATTACH" {
        return None;
    }
    let bytes = sql.as_bytes();
    let spans = wire::scan::scan(sql);
    let at = spans.iter().position(|s| matches!(s.kind, wire::scan::Kind::Str) && bytes[s.start] == b'\'')?;
    let path = &spans[at];
    let after = &sql[path.end..];
    let mut tokens = Vec::new();
    names_in_order(after, &mut tokens);
    match (tokens.first(), tokens.get(1)) {
        (Some(Token::Word(w)), Some(Token::Word(name) | Token::Quoted(name))) if w == b"as" => Some(name.clone()),
        _ => {
            let quoted = sql[path.start + 1..path.end.saturating_sub(1).max(path.start + 1)].replace("''", "'");
            let file = quoted.rsplit(['/', '\\']).next().unwrap_or_default();
            let file = file.split_once(':').map_or(file, |(_, rest)| rest);
            let stem = file.split('.').next().unwrap_or_default();
            (!stem.is_empty()).then(|| stem.to_ascii_lowercase().into_bytes())
        }
    }
}

/// Whether an `ATTACH` replaces a database of its name: `ATTACH OR
/// REPLACE`.
fn replaces(sql: &str) -> bool {
    let mut tokens = Vec::new();
    names_in_order(sql, &mut tokens);
    let word = |at: usize, w: &str| matches!(tokens.get(at), Some(Token::Word(t)) if t == w.as_bytes());
    (0..tokens.len()).find(|&at| word(at, "attach")).is_some_and(|at| word(at + 1, "or") && word(at + 2, "replace"))
}

/// Whether a database is attached as `name`, lowercased, on `conn`.
fn attached(conn: &mut Connection, name: &[u8]) -> bool {
    let name = wire::open::quote_literal(&String::from_utf8_lossy(name));
    conn.query_strings(&format!("SELECT count(*)::VARCHAR FROM duckdb_databases() WHERE lower(database_name) = {name}"))
        .is_ok_and(|rows| rows.first().is_some_and(|n| n != "0"))
}

/// A statement's leading tokens, as `made_name`, `detached_name` and
/// `attached_name` read them.
enum Token {
    Word(Vec<u8>),
    Quoted(Vec<u8>),
    Dot,
    Other,
}

/// The tokens of `sql`'s code and quoted names, lowercased, in order, with
/// comments passed over; reading stops at the first string, which no name
/// `made_name` reads comes after.
fn names_in_order(sql: &str, tokens: &mut Vec<Token>) {
    let is_word = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80;
    let plain = wire::scan::plain_spaces(sql.as_bytes());
    let bytes = &*plain;
    for span in wire::scan::spans(bytes) {
        let text = &bytes[span.start..span.end];
        match span.kind {
            wire::scan::Kind::Code => {
                let mut i = 0;
                while i < text.len() {
                    let c = text[i];
                    if is_word(c) {
                        let start = i;
                        while i < text.len() && is_word(text[i]) {
                            i += 1;
                        }
                        tokens.push(Token::Word(text[start..i].to_ascii_lowercase()));
                        continue;
                    }
                    if c == b'.' {
                        tokens.push(Token::Dot);
                    } else if !c.is_ascii_whitespace() {
                        tokens.push(Token::Other);
                    }
                    i += 1;
                }
            }
            wire::scan::Kind::Str if matches!(text.first(), Some(b'"' | b'`')) => {
                let quote = text[0];
                let inner = text[1..].strip_suffix(&[quote]).unwrap_or(&text[1..]);
                let doubled = [quote, quote];
                let mut name = Vec::with_capacity(inner.len());
                let mut i = 0;
                while i < inner.len() {
                    name.push(inner[i]);
                    i += if inner[i..].starts_with(&doubled) { 2 } else { 1 };
                }
                tokens.push(Token::Quoted(name.to_ascii_lowercase()));
            }
            wire::scan::Kind::LineComment | wire::scan::Kind::BlockComment => {}
            _ => return,
        }
    }
}

/// The gate as a statement holds it: shared, alone, or neither.
/// Held, and asked only which it is; dropped when the statement is over.
#[allow(dead_code)]
enum Passed {
    Open,
    Reading(Share),
    Changing(Alone),
}

/// Pass the gate for `sql`, the statement running on `slot`, in
/// autocommit when `autocommit`. A reader waits on an attach or detach,
/// and gives up when the statement is cancelled while it waits: by its
/// client, by leaving or by a deadline. One that attaches or detaches
/// waits for the readers running, each a catalog read the engine answers
/// from memory or a database's file, and keeps readers that come after it
/// out, each for at most [`PASS_BESIDE`] while it waits for a reader it
/// found; it gives up as a reader does.
///
/// While the names are not all known (`CatalogNames::complete`), every
/// statement that names anything is a reader, and a long one would hold an
/// ATTACH for as long as it runs. So a reader that reads only because they
/// are not known waits for a listing begun (`LEARNING`) and passes as the
/// names it learned say, and one in autocommit is let go of once a listing
/// says it reads nothing (`let_unread_go`); and an ATTACH or DETACH does
/// not keep readers out while it waits, but asks for the names beside them (`learn_beside`), and
/// keeps them out once they are known, or once it has asked for
/// [`ASK_BESIDE`] and asking cannot learn them for now: readers that
/// overlap would otherwise keep it out for as long as they came, and the
/// names may not read for longer, while a SQLite file stays locked.
fn pass_gate(sql: &str, slot: &SlotState, autocommit: bool) -> Option<Passed> {
    let (gate, mut unknown) = catalog_gate_known(sql);
    match gate {
        Gate::Open => Some(Passed::Open),
        Gate::Changes => {
            let asking = Instant::now();
            let keep = || names_complete() || (!learn_beside() && asking.elapsed() >= ASK_BESIDE);
            hold_alone(Some(slot), None, keep).map(Passed::Changing)
        }
        Gate::Reads => {
            while unknown && LEARNING.load(Ordering::SeqCst) > 0 {
                if slot.cancelled() {
                    return None;
                }
                thread::sleep(Duration::from_millis(1));
            }
            if unknown && names_complete() {
                unknown = false;
                if catalog_gate(sql) == Gate::Open {
                    return Some(Passed::Open);
                }
            }
            let unknown = (unknown && autocommit).then(|| (Arc::from(sql), RESHAPED.load(Ordering::SeqCst)));
            wait_share(Some(slot), unknown).map(Passed::Reading)
        }
    }
}

/// The gate shared, for harbor's own reads of the catalog off the pool's
/// executors: the workspace's record, written as the server stops.
pub(crate) fn reading_catalog() -> Share {
    wait_share(None, None).expect("a share harbor asks for is not cancelled")
}

/// The gate held alone, for harbor's own attaching off the pool's
/// executors: the workspace's SQLite files, attached in the background.
pub(crate) fn changing_catalog() -> Alone {
    hold_alone(None, None, || true).expect("harbor's own wait is not cancelled")
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
    ended: &mut bool,
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
    // during either is dropped by the engine, as is one that lands before
    // the statement is submitted. So the slot is asked once more after
    // submission, before anything runs (`Conn::execute`).
    let mut params = params;
    let bound = match conn.bind(last, &mut params) {
        Ok(b) => b,
        Err(e) => {
            let _ = ready.send(Err(on_slot.refuse(conn, e.into_text())));
            return true;
        }
    };
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
    // The VARIANT caster, one per connection, made before the statement
    // runs: making it looks JSON up in the connection's catalog, which must
    // not meet the fetch thread stepping the same connection. Every engine
    // harbor ships has the cast route; one that lacks it can still answer
    // any statement whose result holds no VARIANT, and refuses the rest
    // rather than send display text under a schema line that promised JSON.
    let json = conn.json();
    // A retained statement (see `SlotRun::retained`) is awaited until its
    // first fetch comes back.
    let mut awaiting = false;
    let wanted = |retained| {
        let wanted = on_slot.slot.submitted(retained);
        awaiting = wanted && retained;
        wanted
    };
    let mut stream = match conn.execute(last, bound, wanted) {
        Ok(Some(s)) => s,
        Ok(None) => {
            let _ = ready.send(Err(on_slot.refuse(conn, String::new())));
            return needs_reset;
        }
        Err(e) => {
            // A group's first step failed (`Conn::execute`): what it did
            // is the error's, as at a first fetch below.
            if awaiting && !e.interrupted() {
                on_slot.slot.keep();
            }
            let _ = ready.send(Err(on_slot.refuse(conn, e.into_text())));
            return true;
        }
    };
    let columns = std::mem::take(&mut stream.columns);
    let api = conn.api();
    let json = match json {
        Ok(j) => Some(j),
        Err(e) => {
            if columns.iter().any(|(_, ty)| crate::engine::encode::holds_variant(ty)) {
                if awaiting && stream.stop() {
                    on_slot.slot.keep();
                }
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
        if awaiting && stream.stop() {
            on_slot.slot.keep();
        }
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
        let next = stream.next_chunk();
        // A retained statement's first fetch: a chunk or its end says it
        // ran to its end, and a cancel that fired at it stopped nothing.
        // So does any error but the interrupt, which is the statement's own
        // and may follow work that was kept: a commit is durable before the
        // checkpoint that follows it fails. The client is told that error,
        // and the canceller that its cancel stopped nothing.
        if awaiting && next.as_ref().map_or_else(|e| !e.interrupted(), |_| true) {
            on_slot.slot.keep();
            awaiting = false;
        }
        let fetched = next.and_then(|next| match next {
            None => Ok(None),
            Some(chunk) => chunk.readers(columns.len()).map(|r| Some((chunk, r))),
        });
        let (chunk, readers) = match fetched {
            Ok(Some(pair)) => pair,
            // The end, where the statement is retired. One a canceller
            // reached first is answered as cancelled: the engine can finish
            // a streaming statement before the interrupt lands, and the
            // canceller was told it stopped it, which holds, since a stream
            // keeps nothing of what it read. A retained statement was kept
            // at its first fetch, and no cancel reaches it after that.
            Ok(None) => {
                if !on_slot.finish() {
                    break;
                }
                cut_short = true;
                let refusal = Refusal::cancelled();
                if shape == Shape::Ndjson {
                    push_error(&mut buf, refusal.code, &refusal.message);
                    buf.push('\n');
                    let _ = body.send(std::mem::take(&mut buf).into_bytes());
                    gone = true;
                }
                failure = Some(refusal);
                break;
            }
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
    *ended = !gone && failure.is_none();

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
        let Job { sql, params, shape, id, deadline, reset, ready, body, claims } = job;
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

        // An ATTACH or DETACH runs alone against the catalog's readers
        // (CATALOG_GATE). A reader cancelled while it waits did not run, and
        // its transaction is left aborted, as for a cancel before it began.
        // A view or macro the statement makes is known to the gate first,
        // and the views an ATTACH brings are unknown until they are read.
        learn_from_statement(&sql);
        let brings = brings_views(&sql);
        let probe = detached_name(&sql).or_else(|| attached_name(&sql)).is_some_and(|n| n.starts_with(PROBE.as_bytes()));
        let Some(passed) = pass_gate(&sql, &state, !pinned) else {
            on_slot.finish();
            conn.abort_transaction();
            let _ = ready.send(Err(Refusal::cancelled()));
            continue;
        };
        let _reshaping = reshapes(&sql).then(Reshaping::begin);
        if brings && matches!(passed, Passed::Changing(_)) {
            CATALOG_NAMES.write().unwrap_or_else(PoisonError::into_inner).unsettle();
        }
        // An ATTACH of a name attached already changes it only when it
        // replaces it: `IF NOT EXISTS` leaves it as it was.
        let keeps = claims.is_some() && attached_name(&sql).is_some_and(|name| !replaces(&sql) && attached(&mut conn, &name));
        let mut ended = false;

        // A panic below — an encoder invariant tripping, an FFI metadata
        // assert (the v2 paths return Err rather than panic, so this is the
        // backstop, not the expectation) — must not unwind out of this
        // thread: the worker would find the job channel closed, leave the
        // accept loop, and the slot would be gone for the life of the
        // process, so a handful of such queries would retire every worker.
        // On a panic the slot is retired, the statement's transaction is
        // left aborted, as any statement cut short leaves it, so a COMMIT
        // after it is told; the waiting worker is told (500), and this
        // executor takes the next job. The connection itself is intact (the
        // panic was in Rust-side encoding, not DuckDB's engine), so the next
        // job on a worker resets first.
        let ready_guard = ready.clone();
        needs_reset = match std::panic::catch_unwind(AssertUnwindSafe(|| {
            run_statement(&mut conn, &mut on_slot, sql, params, shape, ready, body, started, &mut ended)
        })) {
            Ok(next_reset) => next_reset,
            Err(_) => {
                on_slot.finish();
                conn.abort_transaction();
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
        // After an ATTACH or DETACH, a probe's but, the views and macros
        // are learned again when they are not all known (`learn_again`); the
        // ones an import or a copy of a database brings, under the gate
        // shared.
        if let Some(claims) = claims {
            let _ = claims.send(ended && !keeps);
        }
        match passed {
            Passed::Changing(alone) if !probe => learn_again(alone),
            passed if brings => {
                let _reading = matches!(passed, Passed::Open).then(reading_catalog);
                learn_on_learner();
            }
            _ => {}
        }
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

    /// An ATTACH that waits keeps new shares out, but a share that waits
    /// half a second passes beside it while a share it found is held, and
    /// not after: what it waits for then is only what came after it.
    #[test]
    fn a_waiting_attach_holds_later_shares_only_while_it_waits_for_earlier_ones() {
        use super::{GateState, PASS_BESIDE};
        let soon = Duration::ZERO;
        let mut gate = GateState::new();
        let long = gate.give();
        assert!(!gate.take_alone(), "held alone beside a share");
        gate.keep();
        assert!(!gate.shares_now(soon), "a share given beside an ATTACH that keeps them out");
        assert!(gate.shares_now(PASS_BESIDE), "a share kept out for as long as the long one runs");
        let beside = gate.give();
        gate.let_go(long);
        assert!(!gate.shares_now(PASS_BESIDE), "a share passed beside after the share found was let go");
        gate.let_go(beside);
        assert!(gate.take_alone());
        gate.keeping -= 1;
        assert!(!gate.shares_now(PASS_BESIDE), "a share beside the gate held alone");
        gate.alone = false;
        assert!(gate.shares_now(soon));
    }

    #[test]
    fn attaching_and_reading_the_catalog_pass_the_gate_apart() {
        use super::{CatalogNames, Gate, gate_of};
        let names = CatalogNames::listed();
        let catalog_gate = |sql| gate_of(sql, &names);
        for sql in [
            "ATTACH '/x/a.sqlite' AS a (TYPE sqlite)",
            "  /* c */ detach a",
            "EXPLAIN ANALYZE ATTACH '/x/a.db'",
            "DETACH DATABASE IF EXISTS a",
        ] {
            assert_eq!(catalog_gate(sql), Gate::Changes, "{sql}");
        }
        for sql in [
            "SELECT * FROM duckdb_tables()",
            "select database_name from DUCKDB_COLUMNS() where 1",
            "SELECT * FROM information_schema.tables",
            "SELECT * FROM sql_auto_complete('SELECT * FROM t')",
            "SELECT * FROM pg_catalog.pg_class",
            "SELECT * FROM pragma_table_info('t')",
            "SELECT * FROM \"duckdb_schemas\"()",
            "SHOW ALL TABLES",
            "describe t",
            "SUMMARIZE t",
            "PRAGMA show_tables",
            "SELECT library_version FROM pragma_version()",
            "SELECT * FROM sqlite_query('a', 'SELECT 1')",
        ] {
            assert_eq!(catalog_gate(sql), Gate::Reads, "{sql}");
        }
        for sql in [
            "SELECT * FROM a.main.t",
            "SELECT 'duckdb_tables()' AS s",
            "SELECT my_duckdb_tables FROM t",
            "SELECT * FROM \"say duckdb_tables\"",
            "INSERT INTO attached VALUES (1)",
            "SELECT * FROM \"select\"",
            "SELECT 0x10",
            "SELECT now()",
        ] {
            assert_eq!(catalog_gate(sql), Gate::Open, "{sql}");
            // Every statement that names anything reads it while the names
            // are not all known.
            assert_eq!(gate_of(sql, &CatalogNames::default()), Gate::Reads, "{sql}");
        }
        // One that names nothing reads nothing, known or not.
        for sql in ["SELECT 42", "select 1 + 2 AS select", "SELECT 1 -- information_schema", "SELECT 'v', 1_000"] {
            assert_eq!(catalog_gate(sql), Gate::Open, "{sql}");
            assert_eq!(gate_of(sql, &CatalogNames::default()), Gate::Open, "{sql}");
        }
        assert_eq!(gate_of("DETACH a", &CatalogNames::default()), Gate::Changes);
        assert_eq!(gate_of("SHOW ALL TABLES", &CatalogNames::default()), Gate::Reads);
    }

    #[test]
    fn a_view_or_macro_whose_body_reads_the_catalog_reads_it() {
        use super::{CatalogNames, Gate, Made, gate_of, made_name};
        let mut names = CatalogNames::listed();
        names.learn(b"v", "CREATE VIEW v AS SELECT * FROM duckdb_tables()");
        names.learn(b"plain", "CREATE VIEW plain AS SELECT 1");
        // Through another view, learned before or after the one it reads.
        names.learn(b"w", "CREATE VIEW w AS SELECT * FROM \"Later\"");
        names.learn(b"later", "CREATE VIEW later AS SELECT * FROM main.V");
        names.learn(b"m", "CREATE MACRO m() AS TABLE SELECT * FROM information_schema.tables");
        names.rename(b"v", b"renamed");
        for sql in ["SELECT * FROM v", "select * from MAIN.\"V\"", "SELECT * FROM w", "FROM m()", "TABLE renamed"] {
            assert_eq!(gate_of(sql, &names), Gate::Reads, "{sql}");
        }
        for sql in ["SELECT * FROM plain", "SELECT 'v'", "SELECT 1 -- v", "SELECT vv FROM t"] {
            assert_eq!(gate_of(sql, &names), Gate::Open, "{sql}");
        }
        // A statement in a string, run by query(), and a table named in one.
        names.learn(b"q", "CREATE VIEW q AS SELECT * FROM query('SELECT * FROM duckdb_tables()')");
        names.learn(b"p", "PREPARE p AS SELECT * FROM duckdb_columns()");
        for sql in [
            "SELECT count(*) FROM query('SELECT * FROM duckdb_tables()')",
            "SELECT count(*) FROM query_table('v')",
            "FROM q",
            "EXECUTE p",
            "SELECT * FROM query /* c */ ('SELECT 1')",
            "SELECT * FROM \"Query\"\n-- c\n('SELECT 1')",
            "SELECT * FROM main.query_table ( 'v' )",
            "SELECT\u{a0}*\u{a0}FROM\u{a0}duckdb_tables()",
        ] {
            assert_eq!(gate_of(sql, &names), Gate::Reads, "{sql}");
        }
        // A column, alias or table named query is a name like any other.
        names.learn(b"cols", "CREATE VIEW cols AS SELECT query, query_table FROM log");
        for sql in [
            "SELECT sum(hash(i)) AS query FROM range(10) t(i)",
            "SELECT query, \"query_table\" FROM log WHERE query = '('",
            "SELECT * FROM query",
            "SELECT query -- (\nFROM log",
            "FROM cols",
        ] {
            assert_eq!(gate_of(sql, &names), Gate::Open, "{sql}");
        }
        // A view named query reads the catalog as any view does.
        names.learn(b"query", "CREATE VIEW query AS SELECT * FROM duckdb_tables()");
        assert_eq!(gate_of("SELECT * FROM query", &names), Gate::Reads);

        let body = |name: &str| Some(Made::Body(name.as_bytes().to_vec()));
        assert_eq!(made_name("CREATE VIEW v AS SELECT 1"), body("v"));
        assert_eq!(made_name("create or replace temp view if not exists db.s.\"My \"\"V\"\"\" AS SELECT 1"), body("my \"v\""));
        assert_eq!(made_name("CREATE /* c */ RECURSIVE VIEW `R`(n) AS SELECT 1"), body("r"));
        assert_eq!(made_name("CREATE MACRO s.m(a) AS TABLE SELECT a"), body("m"));
        assert_eq!(made_name("CREATE FUNCTION f(a) AS a + 1"), body("f"));
        assert_eq!(made_name("PREPARE \"P\" AS SELECT 1"), body("p"));
        assert_eq!(
            made_name("ALTER VIEW IF EXISTS s.v RENAME TO w"),
            Some(Made::Renamed { from: b"v".to_vec(), to: b"w".to_vec() })
        );
        for sql in ["CREATE TABLE v AS SELECT 1", "ALTER VIEW v SET SCHEMA s", "CREATE VIEW 'v' AS SELECT 1", "SELECT 1"] {
            assert_eq!(made_name(sql), None, "{sql}");
        }
    }

    #[test]
    fn the_names_that_read_the_catalog_settle_once_however_many() {
        use super::{CatalogNames, used_names};
        // A chain of twenty thousand views, each over the one before, the
        // first over the catalog, listed last first, as the engine may
        // list them: settled in one pass, where a pass per name is
        // quadratic.
        let mut learned: Vec<_> = (1..20_000)
            .rev()
            .map(|i| (format!("v{i}").into_bytes(), used_names(&format!("CREATE VIEW v{i} AS SELECT * FROM v{}", i - 1))))
            .collect();
        learned.push((b"v0".to_vec(), used_names("CREATE VIEW v0 AS SELECT * FROM duckdb_tables()")));
        learned.push((b"plain".to_vec(), used_names("CREATE VIEW plain AS SELECT * FROM t")));
        let mut names = CatalogNames::default();
        names.learn_all(learned);
        assert_eq!(names.reads.len(), 20_000);
        assert!(names.reads.contains(b"v19999".as_slice()) && !names.reads.contains(b"plain".as_slice()));
        // One learned later over the chain reads it too.
        names.learn(b"top", "CREATE VIEW top AS SELECT * FROM v19999");
        assert!(names.reads.contains(b"top".as_slice()));
    }

    #[test]
    fn an_import_names_the_views_its_schema_makes() {
        use super::{imported_statements, statements_of};
        assert_eq!(
            statements_of("CREATE VIEW a AS SELECT ';';\n-- ; x\nCREATE MACRO m() AS 1 ;\n\n"),
            ["CREATE VIEW a AS SELECT ';'", "\n-- ; x\nCREATE MACRO m() AS 1 "]
        );
        let dir = std::env::temp_dir().join(format!("harbor-import-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("it's")).unwrap();
        std::fs::write(dir.join("it's").join("schema.sql"), "CREATE TABLE t(a INTEGER);\nCREATE VIEW iv AS SELECT * FROM duckdb_tables();\n").unwrap();
        let path = dir.join("it's").to_string_lossy().replace('\'', "''");
        let statements = imported_statements(&format!("IMPORT DATABASE '{path}'"));
        assert_eq!(statements.len(), 2);
        assert!(statements[1].contains("CREATE VIEW iv"));
        assert!(imported_statements("IMPORT DATABASE '/no/such/directory'").is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_sqlite_file_that_is_busy_is_listed_as_last_read_and_one_gone_for_good_is_not() {
        use super::{Busy, CatalogDatabase, STALE, for_good, stale_attached, workspace};
        let here = std::env::current_exe().unwrap().to_string_lossy().to_string();
        let locked = "Failed to prepare query \"SELECT count(*) FROM sqlite_master\": database is locked";
        assert!(!for_good(&here, locked));
        assert!(!for_good("", locked));
        assert!(for_good("/no/such/file.sqlite", locked));
        assert!(for_good(&here, "Failed to prepare query: file is not a database"));
        assert!(for_good(&here, "Unable to open database: unable to open database file"));

        let entry = |name: &str, path: &str, tables: &[&str], error: Option<&str>| wire::catalog::Attached {
            name: name.into(),
            path: Some(path.into()),
            storage: "sqlite".into(),
            error: error.map(str::to_string),
            tables: tables.iter().map(|t| wire::catalog::Table { name: t.to_string(), ..Default::default() }).collect(),
            ..Default::default()
        };
        let last = [
            entry("app", "/x/app.sqlite", &["people"], None),
            entry("gone", "/x/gone.sqlite", &[], Some("/x/gone.sqlite: no such file")),
            entry("moved", "/x/old.sqlite", &["t"], None),
        ];
        let database = |name: &str, path: &str, served: bool| CatalogDatabase {
            name: name.into(),
            path: Some(path.into()),
            storage: if served { "duckdb".into() } else { "sqlite".into() },
            read_only: false,
            served,
            aside: false,
        };
        let databases = [
            database("app", "/x/app.sqlite", false),
            database("main", "/x/main.duckdb", true),
            database("moved", "/x/new.sqlite", false),
            database("since", "/x/since.sqlite", false),
        ];
        let busy = [Busy { name: "app".into(), path: "/x/app.sqlite".into(), why: "database is locked".into() }];
        let unattached = vec![workspace::Unattached {
            record: wire::open::Record { name: "gone".into(), path: "/x/gone.sqlite".into(), storage: "sqlite".into(), read_only: false },
            reason: "/x/gone.sqlite: no such file".into(),
        }];
        let listed: Vec<_> = stale_attached(&last, &databases, &busy, unattached)
            .into_iter()
            .map(|a| (a.name, a.tables.into_iter().map(|t| t.name).collect::<Vec<_>>(), a.error))
            .collect();
        assert_eq!(
            listed,
            [
                ("app".to_string(), vec!["people".to_string()], Some(format!("database is locked: {STALE}"))),
                ("gone".to_string(), vec![], Some("/x/gone.sqlite: no such file".to_string())),
                // Attached from another file since: nothing of the last read is its.
                ("moved".to_string(), vec![], None),
                ("since".to_string(), vec![], None),
            ]
        );
    }

    #[test]
    fn a_detach_names_its_database_and_a_sqlite_attach_brings_no_views() {
        use super::{brings_views, detached_name};
        assert_eq!(detached_name("DETACH app"), Some(b"app".to_vec()));
        assert_eq!(detached_name("/* x */ detach database if exists \"My App\""), Some(b"my app".to_vec()));
        assert_eq!(detached_name("SELECT 1"), None);
        use super::attached_name;
        assert_eq!(attached_name("ATTACH '/x/a.sqlite' AS App (TYPE sqlite)"), Some(b"app".to_vec()));
        assert_eq!(attached_name("attach database if not exists '/x/a.db' as \"My App\""), Some(b"my app".to_vec()));
        assert_eq!(attached_name("ATTACH '/x/Some.File.duckdb'"), Some(b"some".to_vec()));
        assert_eq!(attached_name("ATTACH 'sqlite:/x/b.db' (READ_ONLY)"), Some(b"b".to_vec()));
        assert_eq!(attached_name("DETACH a"), None);
        for sql in [
            "ATTACH '/x/a.duckdb' AS a",
            "ATTACH 'sqlite.duckdb'",
            "ATTACH '/x/a.duckdb' AS sqlite",
            "ATTACH '/x/sqlite' AS \"sqlite\" (READ_ONLY) -- TYPE sqlite",
            "ATTACH '/x/a.duckdb' AS a (TYPE duckdb, READ_ONLY)",
            "IMPORT DATABASE '/x/export'",
            "COPY FROM DATABASE a TO b",
            "explain analyze copy from database a to b (schema)",
        ] {
            assert!(brings_views(sql), "{sql}");
        }
        for sql in [
            "ATTACH '/x/a.db' AS a (TYPE sqlite, READ_ONLY)",
            "ATTACH '/x/a.db' AS a (READ_ONLY, type SQLITE3)",
            "ATTACH '/x/a.db' AS a (TYPE 'sqlite')",
            "ATTACH 'sqlite:/x/a.db' AS a",
            "DETACH a",
            "COPY t FROM 'x.csv'",
            "SELECT 1",
        ] {
            assert!(!brings_views(sql), "{sql}");
        }
    }

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
            super::Shape::Json, ready, body, Instant::now(), &mut false);
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
                deadline: None, reset: false, ready, body, claims: None,
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
                ready, body, Instant::now(), &mut false);
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
                deadline: None, reset: false, ready, body, claims: None }).unwrap();
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

    /// A cancel is answered true only when it stopped its statement and
    /// nothing of it was kept. An autocommit INSERT has run and committed by
    /// the time its first chunk comes back, its RETURNING rows included, so
    /// a cancel the engine did not reach in time is answered false, and the
    /// statement as what it did. Cancels are asked until answered, from a
    /// little later each time, across the statement's life.
    #[test]
    fn a_cancel_is_answered_true_only_when_nothing_was_kept() {
        use std::sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc::sync_channel};
        if crate::engine::engine().is_err() { return; }
        let mut conn = crate::engine::conn::open(std::path::Path::new(":memory:"), &[]).unwrap();
        conn.execute_batch("CREATE TABLE r(k INTEGER, i BIGINT)").unwrap();
        let state = super::SlotState::new(conn.interrupt_handle());
        let (mut stopped, mut kept) = (0, 0);
        for k in 0..400u64 {
            let id = super::next_job_id();
            let returning = if k % 2 == 1 { " RETURNING i" } else { "" };
            let sql = format!("INSERT INTO r SELECT {k}, i FROM range(5000) t(i){returning}");
            state.begin(id, None);
            let over = Arc::new(AtomicBool::new(false));
            let canceller = {
                let (state, over) = (Arc::clone(&state), Arc::clone(&over));
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_micros(k * 13 % 1500));
                    loop {
                        if state.cancel_and_say(id) {
                            return true;
                        }
                        if over.load(Ordering::SeqCst) {
                            return false;
                        }
                    }
                })
            };
            let mut slot = super::OnSlot { slot: &state, done: false };
            let (ready, result) = sync_channel(1);
            let (body, _output) = sync_channel(1);
            super::run_statement(&mut conn, &mut slot, sql, vec![], super::Shape::Json,
                ready, body, Instant::now(), &mut false);
            drop(slot);
            over.store(true, Ordering::SeqCst);
            let said = canceller.join().unwrap();
            let answer = result.recv().unwrap().map_err(|r| r.status);
            let rows = conn.query_strings(&format!("SELECT count(*)::VARCHAR FROM r WHERE k = {k}")).unwrap();
            if said {
                stopped += 1;
                assert_eq!((answer, rows[0].as_str()), (Err(499), "0"), "run {k}: told it was cancelled");
            } else {
                kept += 1;
                assert_eq!((answer, rows[0].as_str()), (Ok(()), "5000"), "run {k}: told it was not");
            }
        }
        // Both answers were given, or the test proved nothing about one.
        assert!(stopped > 0 && kept > 0, "{stopped} stopped, {kept} kept");
    }

    /// A retained statement whose first fetch fails with an error of its own
    /// is answered that error, and its canceller false: only the interrupt
    /// says the cancel stopped it. The slot's interrupt is aimed at another
    /// connection, as one the engine loses is, so the statement runs on to
    /// its error with a cancel fired at it.
    #[test]
    fn a_cancel_is_answered_true_only_for_the_interrupt() {
        use std::sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc::sync_channel};
        if crate::engine::engine().is_err() { return; }
        let mut conn = crate::engine::conn::open(std::path::Path::new(":memory:"), &[]).unwrap();
        let elsewhere = crate::engine::conn::open(std::path::Path::new(":memory:"), &[]).unwrap();
        conn.execute_batch("CREATE TABLE e(i BIGINT)").unwrap();
        let state = super::SlotState::new(elsewhere.interrupt_handle());
        let id = super::next_job_id();
        state.begin(id, None);
        let over = Arc::new(AtomicBool::new(false));
        let canceller = {
            let (state, over) = (Arc::clone(&state), Arc::clone(&over));
            std::thread::spawn(move || {
                while !state.run.lock().unwrap().retained {
                    if over.load(Ordering::SeqCst) {
                        return None;
                    }
                    std::thread::yield_now();
                }
                Some(state.cancel_and_say(id))
            })
        };
        let sql = "INSERT INTO e SELECT CASE WHEN i = 30000000 THEN error('boom') ELSE i END \
                   FROM range(30000001) t(i)";
        let mut slot = super::OnSlot { slot: &state, done: false };
        let (ready, result) = sync_channel(1);
        let (body, _output) = sync_channel(1);
        super::run_statement(&mut conn, &mut slot, sql.into(), vec![], super::Shape::Json,
            ready, body, Instant::now(), &mut false);
        drop(slot);
        over.store(true, Ordering::SeqCst);
        let said = canceller.join().unwrap();
        let answer = result.recv().unwrap().map_err(|r| (r.status, r.message));
        let (status, message) = answer.unwrap_err();
        assert!(status == 400 && message.contains("boom"), "{status} {message}");
        assert_eq!(said, Some(false), "the cancel was answered true for the statement's own error");
        assert_eq!(conn.query_strings("SELECT count(*)::VARCHAR FROM e").unwrap(), ["0"]);
    }

    /// A stop fires the reaper's re-fire itself while it waits for the
    /// executors: a cancel the engine lost would otherwise make it wait
    /// the statement out. The lost cancel is a slot marked cancelled with no
    /// interrupt fired.
    #[test]
    fn a_stop_fires_again_a_cancel_the_engine_lost() {
        use std::sync::{Arc, mpsc::sync_channel};
        if crate::engine::engine().is_err() { return; }
        let mut conn = crate::engine::conn::open(std::path::Path::new(":memory:"), &[]).unwrap();
        let state = super::SlotState::new(conn.interrupt_handle());
        let (ready, result) = sync_channel(1);
        let executor = {
            let state = Arc::clone(&state);
            std::thread::spawn(move || {
                state.begin(super::next_job_id(), None);
                let mut slot = super::OnSlot { slot: &state, done: false };
                let (body, _output) = sync_channel(1);
                super::run_statement(&mut conn, &mut slot,
                    "SELECT count(*) FROM range(1000000000000) t(i) WHERE i % 7 = 3".into(),
                    vec![], super::Shape::Json, ready, body, Instant::now(), &mut false);
                drop(slot);
                Some(conn)
            })
        };
        while state.current_job() == 0 {
            std::thread::yield_now();
        }
        std::thread::sleep(Duration::from_millis(100));
        state.run.lock().unwrap().cancelled = true;
        let started = Instant::now();
        let mut pool = Vec::new();
        let left = super::drain(vec![executor], &[Arc::clone(&state)], &mut pool, Duration::from_secs(8));
        assert!(left.is_empty() && pool.len() == 1, "the stop waited out its patience");
        assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
        assert_eq!(result.recv().unwrap().err().unwrap().status, 499);
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

    /// A retained statement's cancel is told by its first fetch. One that
    /// fired and came too late is cleared, stopped nothing and says so, and
    /// nothing reaches the statement after it; one the statement ended on is
    /// told it stopped it.
    #[test]
    fn a_retained_statement_tells_its_canceller_what_happened() {
        use std::sync::{Arc, OnceLock};
        let mut run = SlotRun::idle();
        run.begin(3, None);
        assert!(run.submitted(true));
        assert_eq!(run.arm(Some(3)), Cancel::Fire);
        let told = Arc::clone(run.told.get_or_insert_with(Arc::<OnceLock<bool>>::default));
        run.keep();
        assert_eq!(told.get(), Some(&false));
        assert_eq!(run.arm(Some(3)), Cancel::Nothing, "a kept statement was cancelled");
        assert!(!run.end(), "a kept statement reported itself cancelled");

        run.begin(4, None);
        assert!(run.submitted(true));
        assert_eq!(run.arm(None), Cancel::Fire);
        let told = Arc::clone(run.told.get_or_insert_with(Arc::<OnceLock<bool>>::default));
        assert!(run.end());
        assert_eq!(told.get(), Some(&true));

        // A cancel that reached the statement before it was submitted keeps
        // it from running, and a slot with nothing running retains nothing.
        run.begin(5, None);
        assert_eq!(run.arm(Some(5)), Cancel::Fire);
        assert!(!run.submitted(true));
        assert!(run.end());
        assert!(run.submitted(true));
        assert!(!run.retained);
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
        let key = |d: &str, s: &str, n: &str| (d.to_string(), s.to_string(), n.to_string());
        let rows = vec![key("shop", "main", "orders"), key("app's", "odd schema", "say \"hi\"")];
        assert_eq!(
            catalog_count_sql(&rows).as_deref(),
            Some(
                "SELECT table_ordinal, row_count FROM (\
SELECT 0::UBIGINT AS table_ordinal, count(*)::UBIGINT AS row_count FROM \"shop\".\"main\".\"orders\" \
UNION ALL SELECT 1::UBIGINT AS table_ordinal, count(*)::UBIGINT AS row_count FROM \
\"app's\".\"odd schema\".\"say \"\"hi\"\"\") AS exact_counts ORDER BY table_ordinal"
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
