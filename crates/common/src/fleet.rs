//! The fleet as one list of rows: desired state (config) reconciled against
//! actual state (the runtime directory).
//!
//! This lives in common because three front ends need the same answer and must
//! not each invent their own. `harbor show` draws it as a table, bare `pilot`
//! draws the same table to say what is openable, and DuckTable draws it as a
//! sidebar. When they disagree about whether a berth is running, one of them is
//! lying to somebody.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::{Connection, FileConfig};
use crate::state::State;

/// Where a berth actually answers — read from its sidecar, never guessed.
///
/// Guessing is how `show` came to print `<runtime>/<name>.sock` for a berth
/// that was bound to TCP: a plausible path, a wrong answer, and no way for the
/// reader to tell. A berth that has not registered has no address, and says so.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Addr {
    /// A unix socket, by absolute path.
    Sock(PathBuf),
    /// host:port, for a berth bound to TCP.
    Tcp(String, u16),
}

impl Addr {
    pub fn read(j: &serde_json::Value) -> Option<Addr> {
        if let Some(s) = j["socket"].as_str() {
            return Some(Addr::Sock(PathBuf::from(s)));
        }
        let port = u16::try_from(j["port"].as_u64()?).ok()?;
        Some(Addr::Tcp(j["bind"].as_str().unwrap_or("127.0.0.1").to_string(), port))
    }

    /// Copy-pasteable, whole: exactly what another process needs to dial this
    /// berth. Both forms speak the same HTTP, so the TCP form is written as
    /// the URL it is rather than as a bare `host:port` you have to dress up.
    pub fn full(&self) -> String {
        match self {
            Addr::Sock(p) => crate::paths::shorten(p),
            Addr::Tcp(host, port) => format!("http://{host}:{port}"),
        }
    }
}

/// How a berth's lock file reads — the cheapest liveness answer there is.
///
/// A bool would be enough for `stop` (where "no evidence" must never be read as
/// "safe to signal") but loses the bit a fleet view needs: a lock file that
/// exists and is unheld is the *normal residue of a clean exit*, and must not
/// be confused with no lock at all.
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum Claim {
    /// No lock file. Nothing has ever claimed this name, or state was swept.
    None,
    /// Someone holds it. The berth is alive — proven, without dialling it.
    Held,
    /// The file is there and nobody holds it. Provably not running.
    Free,
}

pub fn claim_state(lock: &Path) -> Claim {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let Ok(f) = std::fs::OpenOptions::new().read(true).open(lock) else {
            return Claim::None;
        };
        // Taking it proves nobody else has it; dropping f releases immediately.
        let free = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        match free {
            true => Claim::Free,
            false => Claim::Held,
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        match std::fs::OpenOptions::new().read(true).write(true).share_mode(0).open(lock) {
            Ok(_) => Claim::Free,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Claim::None,
            Err(_) => Claim::Held,
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// One berth, from every source that knows anything about it.
pub struct Row {
    pub name: String,
    pub state: State,
    pub pid: Option<u64>,
    pub uptime: Option<String>,
    pub db: String,
    pub addr: Option<Addr>,
    pub note: Option<String>,
}

/// Read the runtime directory once, for both sidecars and locks.
pub fn scan_runtime(home: &Path) -> (BTreeMap<String, serde_json::Value>, BTreeSet<String>) {
    let mut sidecars: BTreeMap<String, serde_json::Value> = Default::default();
    let mut locks = BTreeSet::new();
    let Ok(rd) = std::fs::read_dir(home) else { return (sidecars, locks) };
    for p in rd.filter_map(|e| e.ok().map(|e| e.path())) {
        let Some(stem) = p.file_stem().map(|x| x.to_string_lossy().into_owned()) else { continue };
        match p.extension().and_then(|x| x.to_str()) {
            Some("json") => {
                let v = std::fs::read_to_string(&p)
                    .ok()
                    .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                    .unwrap_or_default();
                sidecars.insert(stem, v);
            }
            Some("lock") => {
                locks.insert(stem);
            }
            _ => {}
        }
    }
    (sidecars, locks)
}

/// Reconcile desired (config) against actual (runtime) into one row per name.
///
/// The flock answers liveness for every row but one, so this makes no network
/// call in the common case: N cheap local opens instead of N round trips, each
/// of which could otherwise cost a 2s read timeout. `probe` is only ever called
/// for the one row shape that cannot be settled locally — a sidecar with no
/// lock file at all — which is why it is a parameter: common has no HTTP client
/// and should not grow one to answer a question it almost never asks.
pub fn reconcile(cfg: &FileConfig, home: &Path, probe: &dyn Fn(&Addr) -> bool) -> Vec<Row> {
    let (sidecars, locks) = scan_runtime(home);
    let configured: BTreeMap<&str, &Connection> = cfg.berths().into_iter().collect();

    let mut names: BTreeSet<String> = Default::default();
    names.extend(configured.keys().map(|k| k.to_string()));
    names.extend(sidecars.keys().cloned());
    names.extend(locks.iter().cloned());

    let mut rows: Vec<Row> = Vec::new();
    for name in names {
        let side = sidecars.get(&name);
        let conf = configured.get(name.as_str()).copied();
        let claim = claim_state(&home.join(format!("{name}.lock")));

        let live_db = side.and_then(|j| j["db"].as_str()).unwrap_or("").to_string();
        let want_db =
            conf.and_then(|c| c.database()).map(|p| p.display().to_string()).unwrap_or_default();
        let addr = side.and_then(Addr::read);

        let mut note = None;
        let state = match (claim, side.is_some(), conf.is_some()) {
            (Claim::Held, true, true) if live_db == want_db => State::Running,
            (Claim::Held, true, true) => {
                note = Some(format!(
                    "config now says {} — harbor stop {name} && harbor start {name}",
                    crate::paths::shorten(Path::new(&want_db))
                ));
                State::Drifted
            }
            (Claim::Held, true, false) => {
                note = Some(
                    "not in your config — a client summoned it, or it was started by hand".into(),
                );
                State::Unmanaged
            }
            // Alive with no registration: mid-boot, or a forget ran under it.
            (Claim::Held, false, _) => {
                note = Some("running but unregistered — starting up, or its sidecar was removed".into());
                State::Unmanaged
            }
            (Claim::Free, true, _) => {
                note = Some(format!(
                    "registry says it is running and the lock says otherwise — harbor forget {name}"
                ));
                State::Dead
            }
            // A lock left by a clean exit is normal residue, not a mess.
            (Claim::Free, false, true) | (Claim::None, false, true) => State::Stopped,
            (Claim::Free, false, false) => {
                note = Some(format!("left by a berth that is gone — harbor forget {name}"));
                State::Stale
            }
            // Sidecar, no lock at all: the only row that has to be dialled.
            (Claim::None, true, _) => match addr.as_ref().is_some_and(probe) {
                true if conf.is_some() && live_db == want_db => State::Running,
                true => State::Unmanaged,
                false => {
                    note = Some(format!("no lock and no answer — harbor forget {name}"));
                    State::Dead
                }
            },
            (Claim::None, false, false) => continue,
        };

        let uptime = side
            .and_then(|j| j["startedAtMs"].as_u64())
            .and_then(|t| now_ms().checked_sub(t))
            .map(|ms| crate::lifetime::humanize(Duration::from_millis(ms)));

        rows.push(Row {
            state,
            pid: side.and_then(|j| j["pid"].as_u64()).filter(|_| state.is_live()),
            uptime: uptime.filter(|_| state.is_live()),
            db: match (live_db.is_empty(), want_db.is_empty()) {
                (false, _) => crate::paths::shorten(Path::new(&live_db)),
                (true, false) => crate::paths::shorten(Path::new(&want_db)),
                _ => "—".into(),
            },
            addr: addr.filter(|_| state.is_live()),
            note,
            name,
        });
    }
    rows.sort_by(|a, b| (a.state.rank(), &a.name).cmp(&(b.state.rank(), &b.name)));
    rows
}

/// The fleet as one table. Shared so that `harbor` and `pilot` cannot drift
/// into two different pictures of the same directory.
#[cfg(feature = "term")]
pub fn table(rows: &[Row]) -> crate::ui::Table {
    use crate::ui::{Cell, Table};
    let mut t = Table::new(["NAME", "STATE", "PID", "UPTIME", "ADDRESS", "DATABASE"]);
    for r in rows {
        t.row([
            Cell::new(&r.name),
            Cell::new(r.state.label()).tone(r.state.level().into()),
            Cell::new(r.pid.map(|p| p.to_string()).unwrap_or("—".into())).right(),
            Cell::new(r.uptime.clone().unwrap_or("—".into())).right(),
            Cell::new(r.addr.as_ref().map(Addr::full).unwrap_or("—".into())),
            Cell::new(&r.db),
        ]);
        if let Some(n) = &r.note {
            t.note(r.state.level().into(), n);
        }
    }
    t
}

/// "2 running, 1 stopped" — the one-line count under the table.
pub fn tally(rows: &[Row]) -> String {
    let mut counts: BTreeMap<&str, usize> = Default::default();
    for r in rows {
        *counts.entry(r.state.word()).or_default() += 1;
    }
    counts.iter().map(|(w, n)| format!("{n} {w}")).collect::<Vec<_>>().join(", ")
}
