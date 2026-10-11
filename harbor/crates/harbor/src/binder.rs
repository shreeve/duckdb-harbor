//! Binding the catalog's views, off the pool and within a budget.
//!
//! `/catalog` binds each view (`SELECT * … LIMIT 0`) so that its columns are
//! the ones it reads as, and binding opens the view's files. A read of a
//! file can wait on something that never answers — a pipe with no writer, a
//! network mount that hangs, a slow remote URL — and the engine's interrupt
//! does not reach a thread inside the operating system's `open`. Bound on a
//! worker's own connection, such a view holds the worker past its client's
//! hang-up and past every deadline, and a few catalog reads take the pool.
//!
//! So views are bound here, on a connection and a thread of their own (the
//! binder), one at a time. Each bind has [`BUDGET`]: the reaper interrupts it
//! at the deadline, as it does any statement, and a client that leaves
//! cancels it. A bind that has not come back a [`GRACE`] after its deadline
//! is not waited on past that: its view is reported as not bound, the binder
//! is let go with its thread and connection, which end once the read returns,
//! and the next bind starts another. At most [`MAX_LET_GO`] are let go at
//! once; past that, views are reported unbound until one returns.
//!
//! A view that reads a pipe, a socket or a device is not bound at all, since
//! a read of it waits on another program and takes what it writes. And a
//! view that reads one regular file, as the views harbor and DuckTable make
//! do, is bound once for each state of that file: while its size, its
//! modification time and its inode are what they were when it last bound,
//! the shape the engine kept from that bind is its shape, and binding again
//! would read the file to learn nothing.

use std::collections::HashMap;
use std::sync::{
    Arc, Mutex, PoisonError, mpsc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crate::{Connection, EXEC_STACK, Refusal, RelationKey, SLOTS, SlotState, WATCH_EVERY, next_job_id, refusal_for};

/// How long one view's bind may take before it is interrupted.
pub(crate) const BUDGET: Duration = Duration::from_secs(2);

/// How long past its deadline a bind is waited on, for the interrupt to land.
const GRACE: Duration = Duration::from_secs(1);

/// How many binders may be let go and still be waiting on a read.
const MAX_LET_GO: usize = 2;

/// The connection binders are cloned from, kept when the pool opens.
static SOURCE: Mutex<Option<Connection>> = Mutex::new(None);

/// The binder, while there is one. Held for the length of one bind, so
/// workers take turns.
static BINDER: Mutex<Option<Binder>> = Mutex::new(None);

/// Binders let go whose reads have not returned.
static LET_GO: AtomicUsize = AtomicUsize::new(0);

struct Binder {
    jobs: mpsc::SyncSender<Request>,
    slot: Arc<SlotState>,
    /// Set when the binder is let go, so its thread, ending, says so.
    let_go: Arc<AtomicBool>,
}

struct Request {
    key: RelationKey,
    /// The view's SQL, as the engine renders it.
    ddl: String,
    /// The statement that binds it.
    probe: String,
    id: u64,
    deadline: Instant,
    reply: mpsc::SyncSender<Result<(), Refusal>>,
}

/// Keep a connection to clone binders from. Called once, when the pool opens.
pub(crate) fn keep_source(conn: Connection) {
    *SOURCE.lock().unwrap_or_else(PoisonError::into_inner) = Some(conn);
}

/// End the binder, as the server stops: its thread ends after the bind it
/// is running, if any.
pub(crate) fn stop() {
    BINDER.lock().unwrap_or_else(PoisonError::into_inner).take();
}

/// The client left while its view was being bound; the bind was cancelled.
pub(crate) struct Departed;

/// Bind the view `key`, whose SQL is `ddl`, with `probe`: `Ok(Ok(()))` when
/// it reads, `Ok(Err(..))` with why when it does not, which its view
/// carries, and `Err(Departed)` when `peer` hung up first.
pub(crate) fn bind(
    peer: &justhttp::Peer,
    key: &RelationKey,
    ddl: &str,
    probe: String,
) -> Result<Result<(), Refusal>, Departed> {
    let mut binder = BINDER.lock().unwrap_or_else(PoisonError::into_inner);
    // A bind its client left is still running, or still waiting on a read:
    // waited on as long as a bind is, then let go.
    if let Some(b) = binder.as_ref()
        && b.slot.current_job() != 0
    {
        let until = Instant::now() + BUDGET + GRACE;
        while b.slot.current_job() != 0 && Instant::now() < until {
            thread::sleep(Duration::from_millis(10));
        }
        if b.slot.current_job() != 0 {
            let_go(&mut binder);
        }
    }
    if binder.is_none() {
        if LET_GO.load(Ordering::SeqCst) >= MAX_LET_GO {
            return Ok(Err(Refusal::sql(format!(
                "not bound: the reads of {MAX_LET_GO} views bound before it have not come back, \
                 and the catalog waits on no more until one does"
            ))));
        }
        match spawn() {
            Ok(b) => *binder = Some(b),
            Err(e) => return Ok(Err(Refusal::sql(format!("not bound: no connection to bind it on: {e}")))),
        }
    }
    let Some(b) = binder.as_ref() else { return Ok(Err(Refusal::sql("not bound: no binder"))) };
    let id = next_job_id();
    let deadline = Instant::now() + BUDGET;
    let (reply, answer) = mpsc::sync_channel(1);
    let request = Request { key: key.clone(), ddl: ddl.to_string(), probe, id, deadline, reply };
    if b.jobs.send(request).is_err() {
        binder.take();
        return Ok(Err(Refusal::sql("not bound: the binder stopped")));
    }
    let mut peer = Some(peer.clone());
    loop {
        match answer.recv_timeout(WATCH_EVERY) {
            Ok(Ok(())) => return Ok(Ok(())),
            // Interrupted at its deadline: the client is still here.
            Ok(Err(refusal)) if refusal.code == wire::code::CANCELLED => return Ok(Err(over_budget())),
            Ok(Err(refusal)) => return Ok(Err(refusal)),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                binder.take();
                return Ok(Err(Refusal::sql("not bound: the binder stopped")));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if peer.take_if(|p| p.closed()).is_some() {
                    b.slot.cancel(Some(id));
                    return Err(Departed);
                }
                if Instant::now() >= deadline + GRACE {
                    let_go(&mut binder);
                    return Ok(Err(over_budget()));
                }
            }
        }
    }
}

fn over_budget() -> Refusal {
    Refusal::sql(format!(
        "not bound: binding it took more than {} s, so the catalog stopped waiting; what it reads \
         did not answer in time",
        BUDGET.as_secs()
    ))
}

/// Stop waiting on the binder. Its slot leaves the reaper's list, and its
/// thread ends once the read it waits on returns.
fn let_go(binder: &mut Option<Binder>) {
    let Some(b) = binder.take() else { return };
    b.let_go.store(true, Ordering::SeqCst);
    LET_GO.fetch_add(1, Ordering::SeqCst);
    SLOTS.lock().unwrap_or_else(PoisonError::into_inner).retain(|s| !Arc::ptr_eq(s, &b.slot));
    eprintln!("harbor: a view's bind did not come back, and the catalog stopped waiting on it");
}

fn spawn() -> Result<Binder, String> {
    let conn = match SOURCE.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
        Some(source) => source.try_clone().map_err(|e| e.to_string())?,
        None => return Err("the pool is not open".to_string()),
    };
    let slot = SlotState::new(conn.interrupt_handle());
    let let_go = Arc::new(AtomicBool::new(false));
    let (jobs, requests) = mpsc::sync_channel::<Request>(1);
    let (state, gone) = (Arc::clone(&slot), Arc::clone(&let_go));
    thread::Builder::new()
        .name("harbor-binder".to_string())
        .stack_size(EXEC_STACK)
        .spawn(move || {
            run(conn, requests, &state);
            if gone.load(Ordering::SeqCst) {
                LET_GO.fetch_sub(1, Ordering::SeqCst);
            }
        })
        .map_err(|e| e.to_string())?;
    // Registered for the reaper, which interrupts a bind at its deadline.
    SLOTS.lock().unwrap_or_else(PoisonError::into_inner).push(Arc::clone(&slot));
    Ok(Binder { jobs, slot, let_go })
}

/// A file a view reads, as the binder last bound it.
#[derive(Clone, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
    inode: u64,
}

/// What is at a view's file.
enum Found {
    /// A regular file, as it stands.
    File(Stamp),
    /// A pipe, a socket or a device.
    Special,
    /// Nothing, a directory, or a path that does not stat: bound as it is.
    Other,
}

fn look(path: &str) -> Found {
    let Ok(meta) = std::fs::metadata(path) else { return Found::Other };
    let kind = meta.file_type();
    if kind.is_file() {
        #[cfg(unix)]
        let inode = std::os::unix::fs::MetadataExt::ino(&meta);
        #[cfg(not(unix))]
        let inode = 0;
        return Found::File(Stamp { len: meta.len(), modified: meta.modified().ok(), inode });
    }
    match kind.is_dir() {
        true => Found::Other,
        false => Found::Special,
    }
}

fn run(mut conn: Connection, requests: mpsc::Receiver<Request>, slot: &SlotState) {
    // Each view that bound, and the file it read as it stood then.
    let mut bound: HashMap<RelationKey, (String, Stamp)> = HashMap::new();
    for Request { key, ddl, probe, id, deadline, reply } in requests {
        let file = file_of(&ddl);
        let found = file.as_deref().map(look);
        let stamp = match (&file, found) {
            (Some(path), Some(Found::Special)) => {
                bound.remove(&key);
                let _ = reply.send(Err(Refusal::sql(format!(
                    "not bound: {path} is a pipe, a socket or a device, which the catalog does not \
                     read, since a read of it waits on another program and takes what it writes"
                ))));
                continue;
            }
            (_, Some(Found::File(stamp))) => Some(stamp),
            _ => None,
        };
        if let Some(stamp) = &stamp
            && bound.get(&key).is_some_and(|(sql, was)| *sql == ddl && was == stamp)
        {
            let _ = reply.send(Ok(()));
            continue;
        }
        let answer = match slot.begin(id, Some(deadline)) {
            true => Err(Refusal::cancelled()),
            false => {
                let ran = conn.execute_batch(&probe).map_err(|e| e.into_text());
                let cancelled = slot.end();
                if cancelled {
                    // An interrupt can outlast the statement it stopped.
                    let _ = conn.reset();
                }
                ran.map_err(|message| refusal_for(cancelled, message))
            }
        };
        match (&answer, stamp) {
            (Ok(()), Some(stamp)) => {
                bound.insert(key, (ddl, stamp));
            }
            _ => {
                bound.remove(&key);
            }
        }
        let _ = reply.send(answer);
    }
}

/// The one file a view reads, when the view is a reader's call over a path
/// and nothing else, as harbor and DuckTable make one and as the engine
/// renders it: `CREATE VIEW sales AS SELECT * FROM read_csv('/x/sales.csv');`.
/// The path is the literal's, its quotes undoubled and, for the readers that
/// take a pattern, its glob brackets undone. None for any other view, for a
/// path that is still a pattern, and for one that is not absolute (a URL, or
/// a path the server's directory would decide).
fn file_of(ddl: &str) -> Option<String> {
    let text = ddl.trim_end();
    let text = text.strip_suffix(';').unwrap_or(text).trim_end();
    let text = text.strip_suffix(')')?.trim_end();
    let last = *wire::scan::scan(text).last()?;
    if last.kind != wire::scan::Kind::Str || !last.terminated || last.end != text.len() || !text[last.start..].starts_with('\'') {
        return None;
    }
    let head = text[..last.start].trim_end().strip_suffix('(')?.trim_end();
    let function = ["read_csv", "read_parquet", "read_json", "read_xlsx", "st_read"].into_iter().find(|f| {
        head.len()
            .checked_sub(f.len())
            .and_then(|at| head.get(at..))
            .is_some_and(|tail| tail.eq_ignore_ascii_case(f))
    })?;
    let words: Vec<&str> = head[..head.len() - function.len()].split_ascii_whitespace().collect();
    let before = words.len().checked_sub(4).map(|at| &words[at..])?;
    if !before.iter().zip(["AS", "SELECT", "*", "FROM"]).all(|(w, want)| w.eq_ignore_ascii_case(want)) {
        return None;
    }
    let literal = text[last.start + 1..last.end - 1].replace("''", "'");
    let path = match function {
        "st_read" => literal,
        _ => unglob(&literal)?,
    };
    let absolute = path.starts_with('/')
        || (path.len() > 2 && path.as_bytes()[0].is_ascii_alphabetic() && path[1..].starts_with(":\\"))
        || (path.len() > 2 && path.as_bytes()[0].is_ascii_alphabetic() && path[1..].starts_with(":/"));
    absolute.then_some(path)
}

/// A glob pattern that matches one path, as that path: `[*]`, `[?]` and
/// `[[]` undone. None when anything in it is still a pattern.
fn unglob(pattern: &str) -> Option<String> {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '[' => {
                let escaped = chars.next()?;
                if chars.next()? != ']' || !matches!(escaped, '*' | '?' | '[') {
                    return None;
                }
                out.push(escaped);
            }
            '*' | '?' => return None,
            _ => out.push(c),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::{file_of, unglob};

    #[test]
    fn a_view_over_one_file_is_known_by_that_file() {
        let csv = "CREATE VIEW sales AS SELECT * FROM read_csv('/x/it''s [[]1][*][?].csv');";
        assert_eq!(file_of(csv).as_deref(), Some("/x/it's [1]*?.csv"));
        assert_eq!(file_of("CREATE VIEW p AS SELECT * FROM st_read('/m/p [x].shp');").as_deref(), Some("/m/p [x].shp"));
        assert_eq!(
            file_of("create view \"a AS b\" as select * from READ_PARQUET ( '/p.parquet' ) ;").as_deref(),
            Some("/p.parquet")
        );
        assert_eq!(file_of("CREATE VIEW b AS SELECT * FROM read_xlsx('C:\\x\\b.xlsx');").as_deref(), Some("C:\\x\\b.xlsx"));
        assert_eq!(file_of("CREATE VIEW \"\u{130}\u{130}\" AS SELECT * FROM read_json('/j.json');").as_deref(), Some("/j.json"));
    }

    #[test]
    fn any_other_view_is_bound_every_time() {
        for ddl in [
            "CREATE VIEW lake AS SELECT * FROM read_parquet('/l/**/*.parquet', (hive_partitioning = true));",
            "CREATE VIEW s AS SELECT * FROM read_csv('/x/a*.csv');",
            "CREATE VIEW s AS SELECT * FROM read_csv('/x/a.csv') WHERE id > 1;",
            "CREATE VIEW s AS SELECT * FROM t, read_csv('/x/a.csv');",
            "CREATE VIEW s AS SELECT id FROM read_csv('/x/a.csv');",
            "CREATE VIEW s AS SELECT * FROM read_csv('x/a.csv');",
            "CREATE VIEW s AS SELECT * FROM read_csv('https://h/a.csv');",
            "CREATE VIEW s AS SELECT * FROM read_csv(E'/x/a.csv');",
            "CREATE VIEW s AS SELECT * FROM my_read_csv('/x/a.csv');",
            "CREATE VIEW s AS SELECT * FROM t;",
            "CREATE VIEW s AS SELECT 42;",
        ] {
            assert_eq!(file_of(ddl), None, "{ddl}");
        }
    }

    #[test]
    fn a_pattern_is_a_path_only_when_every_glob_character_is_bracketed() {
        assert_eq!(unglob("/a/[[]x][*]").as_deref(), Some("/a/[x]*"));
        assert_eq!(unglob("/a/x?.csv"), None);
        assert_eq!(unglob("/a/[ab].csv"), None);
        assert_eq!(unglob("/a/[").as_deref(), None);
    }
}
