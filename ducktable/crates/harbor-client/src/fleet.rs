//! The fleet as a GUI sees it: every database the config or a live socket
//! knows, how to dial it, and what state it is honestly in.
//!
//! Truth comes the same way `harbor` (bare) finds it: the runtime dir is
//! scanned for `*.sock` and each socket answers `GET /info` — the listening
//! socket IS the registration, there is no sidecar, lock file, or registry
//! to read. A held connection is presence itself, so there is nothing to
//! pulse and nothing to reconcile. This file layers on what only this client
//! wants: config-named remotes, size on disk, and the whole connection half
//! (Conn, connect).
//!
//! The lifecycle law is harbor's own: **a detached start is ephemeral — it
//! lives while anyone is connected; an attached (or bare) start is persistent
//! — it lives until stopped.** DuckTable owns one quiet anchor connection for
//! as long as a database is open, so an ephemeral server stays present between
//! its otherwise one-shot requests and retires when the database closes.

use crate::http::{Failure, Transport};
use harbor_common::State;
use harbor_common::config;
use harbor_common::paths::{self, runtime_dir};
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// One sidebar row: a database's honest state, plus the size on disk only
/// a GUI wants.
#[derive(Debug, Clone)]
pub struct Survey {
    pub name: String,
    pub state: State,
    /// On your list — a `[connection.*]` in config.toml. A live server that
    /// is not in config (a bare-spawned one) is running-but-unattached.
    pub attached: bool,
    /// A login item exists for this berth — the menu's Autostart checkmark.
    pub autostart: bool,
    /// The database file, when this row is a local berth (not a remote). What
    /// the lifecycle verbs target, and what a click on the row connects to
    /// (`connect_file`): a row is dialed as what it shows, never by looking
    /// its name up again. `None` is a configured remote, dialed by name
    /// (`connect_remote`).
    pub path: Option<PathBuf>,
    /// A human-readable note for a row that needs one: where a remote
    /// connects, and which database a row is when another shares its name.
    pub note: Option<String>,
    /// Size on disk (data file + WAL) — knowable without a connection,
    /// so stopped databases answer too.
    pub size: Option<u64>,
    /// The harbor version a running server reports (`None` when stopped, or
    /// when its server was found by `/ready` alone). Compared against the
    /// installed binary to decide whether the row is outdated.
    pub version: Option<String>,
    /// Whether a running server self-retires when its last client leaves — the
    /// mode a restart must preserve. Meaningless for stopped or remote rows.
    pub ephemeral: bool,
}

/// The whole survey: the rows, and the one thing a GUI must not eat — a
/// config the loader refused. A stderr line is invisible under a window;
/// an empty sidebar with no reason reads as "harbor is broken".
pub struct Fleet {
    pub rows: Vec<Survey>,
    pub warning: Option<String>,
}

/// db file + its `.wal`, when the file exists.
fn disk_size(db: &Path) -> Option<u64> {
    let main = std::fs::metadata(db).ok()?.len();
    let mut wal = db.as_os_str().to_owned();
    wal.push(".wal");
    Some(main + std::fs::metadata(wal).map(|m| m.len()).unwrap_or(0))
}

/// A live server, as its socket tells it: `/info` is the identity document.
struct Live {
    name: String,
    db: PathBuf,
    sock: PathBuf,
    /// The harbor version this server is running — the yardstick for whether
    /// it is outdated relative to the installed binary.
    version: String,
    /// Whether it self-retires when its last client leaves, so a restart can
    /// bring it back in the same lifetime mode.
    ephemeral: bool,
}

/// Every server listening right now, found the way bare `harbor` finds
/// them. A socket nothing answers on is left where it is: sweeping residue
/// is harbor's job, and this is a read-only view. So is a server that
/// answers without saying which file it serves, since a local row is dialed
/// by its file.
fn discover() -> Vec<Live> {
    let Ok(runtime) = runtime_dir() else { return Vec::new() };
    crate::http::discover(&runtime, false)
        .into_iter()
        .filter_map(|found| {
            let info: wire::InfoResponse = serde_json::from_value(found.info?).ok()?;
            (!info.database.is_empty()).then(|| Live {
                name: info.name,
                db: PathBuf::from(info.database),
                sock: found.sock,
                version: info.harbor_version,
                ephemeral: info.ephemeral,
            })
        })
        .collect()
}

/// The config, with a GUI-honest error contract: absent is fine, refused or
/// invalid is a fact to surface, never to fall through — one typo would
/// otherwise blank the sidebar with a stderr line nobody sees.
fn load_config() -> Result<config::FileConfig, String> {
    match config::load() {
        Ok(c) => Ok(c),
        Err(config::Error::Missing(_)) => Ok(Default::default()),
        Err(e) => Err(e.to_string()),
    }
}

/// One row before anything is probed: what it is, and so what a click on
/// it connects to.
#[derive(Debug, PartialEq)]
enum Planned<'a> {
    /// A live server, by its place in the discovered list. `attached` when a
    /// config berth names the file it serves.
    Live { ix: usize, attached: bool },
    /// A config berth no live server answers for.
    Berth { name: &'a str, db: PathBuf },
    /// A config remote.
    Remote { name: &'a str },
}

/// Which rows a config and the live servers make. A row is the database it
/// shows, and names do not decide that: a live server is the file it serves
/// whatever the config calls by its name, a config berth is hidden only
/// when a live server serves that same file, and a remote always has its
/// row. So a local `medlabs.duckdb` and a remote named `medlabs` are two
/// rows, each connecting to itself. `sock_of` maps a database file to the
/// socket its server listens on, which is how a file is recognized under
/// any spelling of its path.
fn plan<'a>(
    cfg: &'a config::FileConfig,
    live: &[Live],
    sock_of: &dyn Fn(&Path) -> Option<PathBuf>,
) -> Vec<Planned<'a>> {
    let berths: Vec<(&str, PathBuf, Option<PathBuf>)> = cfg
        .berths()
        .into_iter()
        .filter_map(|(name, entry)| {
            let db = entry.database()?;
            let sock = sock_of(&db);
            Some((name, db, sock))
        })
        .collect();
    let serves = |l: &Live, db: &Path, sock: &Option<PathBuf>| {
        same_file(&l.db, db) || sock.as_ref().is_some_and(|s| *s == l.sock)
    };
    let mut out: Vec<Planned> = live
        .iter()
        .enumerate()
        .map(|(ix, l)| Planned::Live {
            ix,
            attached: berths.iter().any(|(_, db, sock)| serves(l, db, sock)),
        })
        .collect();
    for (name, db, sock) in &berths {
        if !live.iter().any(|l| serves(l, db, sock)) {
            out.push(Planned::Berth { name, db: db.clone() });
        }
    }
    out.extend(cfg.remotes().into_iter().map(|(name, _)| Planned::Remote { name }));
    out
}

/// Every database a live socket or the config knows, sorted by name, a
/// local row ahead of a remote that shares its name. Live servers carry
/// their own `/info` name; config entries contribute the stopped rows and
/// the remotes. An unreadable config contributes a warning instead of
/// silently contributing nothing.
pub fn survey() -> Fleet {
    let (cfg, mut warning) = match load_config() {
        Ok(c) => (c, None),
        Err(e) => (Default::default(), Some(e)),
    };
    if warning.is_none()
        && let bad = cfg.malformed()
        && !bad.is_empty()
    {
        warning = Some(format!(
            "[connection.{}] needs exactly one of url or path",
            bad.join("], [connection.")
        ));
    }

    let live = discover();
    let home = runtime_dir().ok();
    let sock_of =
        |db: &Path| home.as_ref().and_then(|home| paths::socket_for(home, db).ok());
    let mut out: Vec<Survey> = Vec::new();
    for row in plan(&cfg, &live, &sock_of) {
        out.push(match row {
            Planned::Live { ix, attached } => {
                let l = &live[ix];
                Survey {
                    name: l.name.clone(),
                    state: State::Running,
                    attached,
                    // A login item is filed by name, and is this row's only
                    // when it runs this file.
                    autostart: harbor_common::autostart::keeps(&l.db, &l.name),
                    path: Some(l.db.clone()),
                    note: None,
                    size: disk_size(&l.db),
                    version: Some(l.version.clone()),
                    ephemeral: l.ephemeral,
                }
            }
            // No live server named this file in its `/info`. It is running
            // all the same if the socket derived from its path answers.
            Planned::Berth { name, db } => {
                let running = sock_of(&db).is_some_and(|sock| sock_ready(&sock));
                Survey {
                    name: name.to_string(),
                    state: if running { State::Running } else { State::Stopped },
                    attached: true,
                    autostart: harbor_common::autostart::keeps(&db, name),
                    size: disk_size(&db),
                    path: Some(db),
                    note: None,
                    // A server we found only by its ready socket (not `/info`)
                    // reports no version — treat it as unknown, never outdated.
                    version: None,
                    ephemeral: false,
                }
            }
            // Remotes have no local state at all; a probe answers for them.
            Planned::Remote { name } => {
                // Surveying must never open SSH sessions. A tunneled database is
                // dialed only when the user selects it; while selected, app.rs folds
                // the already-connected phase back into this row as Running.
                let target = cfg
                    .get(name)
                    .and_then(|entry| entry.url.as_deref())
                    .and_then(|url| http_target(url).ok());
                let transport = target
                    .as_ref()
                    .filter(|target| target.is_local())
                    .map(|target| Transport::Tcp(target.addr()));
                let alive = transport.as_ref().is_some_and(crate::http::ready);
                // Best-effort version, so the card can note a remote running behind
                // your own binary. Informational only — you cannot restart a remote
                // from here, so this never counts toward the upgrade badge.
                let version = alive
                    .then(|| transport.as_ref().and_then(|t| info_of(t, Duration::from_millis(800)).ok()))
                    .flatten()
                    .map(|i| i.harbor_version);
                Survey {
                    name: name.to_string(),
                    state: if alive { State::Running } else { State::Stopped },
                    attached: true,
                    autostart: false, // a remote has no local login item
                    path: None,
                    note: target
                        .as_ref()
                        .filter(|target| !target.is_local())
                        .map(|target| format!("Connects over SSH to {}", target.host)),
                    size: None,
                    version,
                    ephemeral: false,
                }
            }
        });
    }

    out.sort_by(|a, b| (&a.name, a.path.is_none()).cmp(&(&b.name, b.path.is_none())));
    if let Some(clash) = name_clashes(&mut out)
        && warning.is_none()
    {
        warning = Some(clash);
    }
    Fleet { rows: out, warning }
}

/// Rows that share a name are different databases. Each says which it is in
/// its note, and the sidebar's warning line says the names repeat. Takes
/// rows sorted by name.
fn name_clashes(rows: &mut [Survey]) -> Option<String> {
    let mut repeated = Vec::new();
    for ix in 0..rows.len() {
        let before = ix > 0 && rows[ix - 1].name == rows[ix].name;
        let after = rows.get(ix + 1).is_some_and(|next| next.name == rows[ix].name);
        if !(before || after) {
            continue;
        }
        if !before {
            repeated.push(rows[ix].name.clone());
        }
        let which = match &rows[ix].path {
            Some(path) => format!("The file {} on this machine", paths::shorten(path)),
            None => "The remote database in your config".to_string(),
        };
        rows[ix].note = Some(match rows[ix].note.take() {
            Some(note) => format!("{which}. {note}"),
            None => which,
        });
    }
    (!repeated.is_empty()).then(|| {
        format!(
            "more than one database is named {} — hover a row to see which it is",
            repeated.join(", ")
        )
    })
}

/// A unix socket that exists and answers /ready.
fn sock_ready(sock: &Path) -> bool {
    sock.exists() && crate::http::ready(&Transport::Unix(sock.to_path_buf()))
}

/// Whether two paths name one database file, however each is spelled.
fn same_file(a: &Path, b: &Path) -> bool {
    a == b
        || match (paths::canonical_db(a), paths::canonical_db(b)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
}

/// A dialable connection to one database.
#[derive(Clone)]
pub struct Conn {
    pub name: String,
    /// The database file this connection serves, when it is one on this
    /// machine. `None` is a configured remote. A row and a connection are
    /// the same database only when they agree on this as well as the name.
    pub db: Option<PathBuf>,
    transport: Transport,
    /// Presence is shared with every clone, just like the tunnel. The final
    /// clone closes it, allowing an ephemeral Harbor server to retire.
    /// `None` for a connection that asks once and lets go.
    #[allow(dead_code)]
    anchor: Option<Arc<Presence>>,
    /// True when this connect raised the server (worth a status line).
    /// A summoned server is an ephemeral `start` — it self-retires once its
    /// last client disconnects, so closing the window that opened it lets it
    /// go. The explicit Start action is the way to keep a server up.
    pub summoned: bool,
    /// Every clone participating in this database connection shares the
    /// tunnel. The final clone closes SSH, so an in-flight query cannot lose
    /// its route merely because the window switched databases.
    tunnel: Option<Arc<SshTunnel>>,
}

/// An anchor, let go of off the thread that drops it. Dropping an anchor
/// waits for its thread, which a renewal in progress holds for as long as
/// its read timeout, and the last clone of a connection often dies on the
/// UI thread.
struct Presence(Option<crate::http::Anchor>);

impl Drop for Presence {
    fn drop(&mut self) {
        if let Some(anchor) = self.0.take() {
            let _ = std::thread::Builder::new().name("harbor-unmoor".into()).spawn(move || drop(anchor));
        }
    }
}

fn anchor(transport: &Transport) -> std::io::Result<Option<Arc<Presence>>> {
    crate::http::hold(transport).map(|anchor| Some(Arc::new(Presence(Some(anchor)))))
}

impl Conn {
    fn plain(name: String, transport: Transport, summoned: bool) -> Result<Self, String> {
        let anchor = anchor(&transport).map_err(|e| format!("connecting to Harbor: {e}"))?;
        Ok(Self { name, db: None, transport, anchor, summoned, tunnel: None })
    }

    /// This connection as the local database file it serves.
    fn serving(mut self, db: &Path) -> Self {
        self.db = Some(db.to_path_buf());
        self
    }

    fn tunneled(name: String, tunnel: SshTunnel) -> Result<Self, String> {
        let transport = Transport::Unix(tunnel.sock.clone());
        let anchor = anchor(&transport).map_err(|e| format!("connecting to Harbor through SSH: {e}"))?;
        Ok(Self {
            name,
            db: None,
            transport,
            anchor,
            summoned: false,
            tunnel: Some(Arc::new(tunnel)),
        })
    }

    pub fn transport(&self) -> Result<&Transport, String> {
        if let Some(tunnel) = &self.tunnel {
            tunnel.ensure_running()?;
        }
        Ok(&self.transport)
    }
}

/// Connect to the configured remote called `name`, and to nothing else: a
/// config berth or a live local server of that name is not it. There is no
/// connecting by name alone. A name can belong to a local file and to a
/// remote at once, so a caller says which it means: this, or `connect_file`.
pub fn connect_remote(name: &str) -> Result<Conn, String> {
    // One name law for the whole fleet: harbor normalizes every name it
    // mints, so every lookup normalizes too.
    let name = harbor_common::normalize(name)?;
    // A refused config must not be answered around.
    let cfg = load_config()?;
    let entry = remote_entry(&cfg, &name)?;
    dial_remote(name, entry)
}

/// The config's remote entry called `name`. A berth of that name is a file
/// on this machine, and is refused: it is not what a remote row shows.
fn remote_entry<'a>(
    cfg: &'a config::FileConfig,
    name: &str,
) -> Result<&'a config::Connection, String> {
    match cfg.get(name) {
        Some(entry) if entry.kind() == config::Kind::Remote => Ok(entry),
        _ => Err(format!("no remote database named {name:?} in the config")),
    }
}

/// Dial a remote entry: directly when its url is this machine, through an
/// SSH tunnel otherwise.
fn dial_remote(name: String, entry: &config::Connection) -> Result<Conn, String> {
    let url = entry
        .url
        .as_deref()
        .ok_or_else(|| format!("config entry {name:?} has no url"))?;
    let target = http_target(url)?;
    if !target.is_local() {
        validate_ssh_host(&target.host)?;
        return Conn::tunneled(name, open_tunnel(&target.host, target.port)?);
    }
    Conn::plain(name, Transport::Tcp(target.addr()), false)
}

/// Connect to the server of one database file on this machine, under the
/// name its row shows. The path is the whole identity: the config is not
/// consulted, so no entry that shares the name can redirect the connection.
/// A stopped database is started on demand, as an ephemeral server.
pub fn connect_file(name: &str, db: &Path) -> Result<Conn, String> {
    let db = paths::canonical_db(db).map_err(|e| format!("{}: {e}", db.display()))?;
    serve_file(name.to_string(), &db)
}

/// Connect to the server of one database file only if it is already
/// running, to ask it one thing: the sidebar's table counts. It never
/// starts a server, and holds no anchor, since the server is up and the
/// answer is all it wants.
pub fn join_file(name: &str, db: &Path) -> Option<Conn> {
    let db = paths::canonical_db(db).ok()?;
    let sock = paths::socket_for(&runtime_dir().ok()?, &db).ok()?;
    sock_ready(&sock).then(|| Conn {
        name: name.to_string(),
        db: Some(db),
        transport: Transport::Unix(sock),
        anchor: None,
        summoned: false,
        tunnel: None,
    })
}

/// Join the server on `db`, or summon one when none answers. Spawning over
/// a live server would only lose DuckDB's file-lock race and read as a
/// failure.
fn serve_file(name: String, db: &Path) -> Result<Conn, String> {
    let sock = paths::socket_for(&runtime_dir()?, db)?;
    if let Some(conn) = join(&name, db, &sock, false)? {
        return Ok(conn);
    }
    // Nothing serves the file yet: summon an ephemeral server — it
    // self-retires when this window's connection drops, since opening a
    // database is not a request to keep it running. Two windows can race
    // one summon; DuckDB's file lock lets exactly one server win, and the
    // loser's socket is the winner's.
    summon(db, &sock, true)?;
    join(&name, db, &sock, true)?.ok_or_else(|| format!("harbor never answered for {}", db.display()))
}

/// The server answering on `sock`, joined, or `None` when nothing answers
/// there. One that answers and then refuses the connection's anchor says
/// why, rather than reading as nothing serving the file.
fn join(name: &str, db: &Path, sock: &Path, summoned: bool) -> Result<Option<Conn>, String> {
    if !sock_ready(sock) {
        return Ok(None);
    }
    Conn::plain(name.to_string(), Transport::Unix(sock.to_path_buf()), summoned).map(|conn| Some(conn.serving(db)))
}

/// Open a database FILE directly — the File→Open / drag-drop door. No
/// config consulted: the path itself is the identity. Canonicalized first,
/// so every spelling of one file meets the same server, then joined or
/// summoned like any file (`serve_file`).
pub fn connect_path(db: &Path) -> Result<Conn, String> {
    let db = paths::canonical_db(db).map_err(|e| format!("{}: {e}", db.display()))?;
    // The stem-derived name harbor itself would mint for this path, which
    // labels the connection; the server's /info answers with its own truth
    // on the next refresh.
    let name = db
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("no usable name in {}", db.display()))
        .and_then(harbor_common::paths::normalize)?;
    serve_file(name, &db)
}

/// Stop the server of one database file: POST /shutdown to its live socket,
/// if one answers, and return once it has gone, so the refresh that follows
/// does not find it. The counterpart to spawn-on-open — the GUI can close
/// what it opened. Idempotent and never-spawning: a file with nothing
/// running is Ok(()), the same as a Stop that raced the server's own
/// departure. The file is the whole target, as for `connect_file`: a name
/// can belong to two databases, and stopping by name could shut down the
/// other one.
pub fn stop(db: &Path) -> Result<(), String> {
    let db = paths::canonical_db(db).map_err(|e| format!("{}: {e}", db.display()))?;
    let own = paths::socket_for(&runtime_dir()?, &db).ok();
    for sock in stop_targets(&db, own, &discover()) {
        if crate::http::shutdown(&sock).map_err(|e| format!("stop {}: {e}", db.display()))? {
            break;
        }
    }
    Ok(())
}

/// The sockets a Stop of `db` may send its shutdown to: the one derived from
/// the file's path, and that of any live server whose `/info` names the same
/// file, which covers a server listening on a socket named another way. A
/// server on another file is never one of them, whatever it is called.
fn stop_targets(db: &Path, own: Option<PathBuf>, live: &[Live]) -> Vec<PathBuf> {
    let mut socks: Vec<PathBuf> = own.into_iter().collect();
    for l in live {
        if same_file(&l.db, db) && !socks.contains(&l.sock) {
            socks.push(l.sock.clone());
        }
    }
    socks
}

/// Start a persistent server for this database, if one is not already up.
pub fn start(db: &Path) -> Result<(), String> {
    let canon = paths::canonical_db(db)?;
    let sock = paths::socket_for(&runtime_dir()?, &canon)?;
    if sock_ready(&sock) {
        return Ok(()); // already running
    }
    summon(&canon, &sock, false)
}

/// Add this database to your list (config.toml): membership is what makes a
/// started server persistent.
pub fn attach(db: &Path) -> Result<(), String> {
    harbor_common::membership::attach(db).map(|_| ())
}

/// Remove this database from your list.
pub fn detach(db: &Path) -> Result<(), String> {
    harbor_common::membership::detach(db).map(|_| ())
}

/// Arm or disarm the login item. Arming attaches the database too (autostart
/// needs it on your list) but never starts it — running is the Start/Stop
/// axis's business; disarming leaves membership and the running server alone.
pub fn set_autostart(db: &Path, on: bool) -> Result<(), String> {
    let name = harbor_common::membership::name_for(db)?;
    if on {
        harbor_common::membership::attach(db)?;
        harbor_common::autostart::arm(db, &name)
    } else if harbor_common::autostart::keeps(db, &name) {
        harbor_common::autostart::remove(&name).map(|_| ())
    } else {
        // The item filed under this name runs another database, or there
        // is none: this one has nothing to disarm.
        Ok(())
    }
}

/// Resolve the `harbor` binary this process spawns and probes. A Finder-
/// launched app inherits launchd's PATH — /usr/bin:/bin and friends — which
/// lacks every directory harbor actually installs into, so probe the usual
/// homes before trusting a bare PATH lookup, or drag-and-drop spawns work from
/// a terminal and fail from the Dock.
fn harbor_bin() -> String {
    std::env::var("HARBOR_BIN")
        .ok()
        .or_else(|| {
            let home = std::env::var("HOME").ok()?;
            [
                format!("{home}/.local/bin/harbor"),
                "/usr/local/bin/harbor".to_string(),
                "/opt/homebrew/bin/harbor".to_string(),
            ]
            .into_iter()
            .find(|p| std::fs::metadata(p).is_ok())
        })
        .unwrap_or_else(|| "harbor".to_string())
}

/// The version of the `harbor` binary this process would spawn — the yardstick
/// for "is a running server outdated". `harbor version` prints `harbor X.Y.Z`;
/// we take the last whitespace-delimited token. `None` if the binary cannot be
/// run, in which case nothing is ever judged outdated.
pub fn installed_harbor_version() -> Option<String> {
    let out = std::process::Command::new(harbor_bin()).arg("version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace().last().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Dotted-numeric version compare: is `running` strictly older than
/// `installed`? Each component is parsed up to its first non-digit, missing
/// components read as 0, and anything unparseable sorts as 0 — so a malformed
/// version is never judged outdated (we do not nag on garbage).
pub fn version_older(running: &str, installed: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        v.trim().trim_start_matches('v')
            .split('.')
            .map(|p| {
                p.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0)
            })
            .collect()
    }
    let (a, b) = (parts(running), parts(installed));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x < y;
        }
    }
    false
}

/// `GET /info` on a transport: the server's identity.
fn info_of(t: &Transport, patience: Duration) -> Result<wire::InfoResponse, Failure> {
    crate::http::call(t, &wire::endpoint::INFO, None, patience)
}

/// Restart a running local server so it comes back on the current binary:
/// stop it, wait until it has gone and let go of DuckDB's file lock (the one
/// thing a hand-typed `stop; start` gets wrong), then summon it again in the
/// same lifetime mode. The one-click upgrade path.
pub fn restart(db: &Path, ephemeral: bool) -> Result<(), String> {
    let canon = paths::canonical_db(db)?;
    let sock = paths::socket_for(&runtime_dir()?, &canon)?;
    crate::http::shutdown(&sock).map_err(|e| format!("stop {}: {e}", db.display()))?;
    summon(&canon, &sock, ephemeral)
}

/// Summon through harbor's own front door, `harbor <db> start`, and wait
/// for it to answer on `sock`. `ephemeral` makes the server self-retire once
/// its last client disconnects: the implicit open-a-database path, where a
/// server nobody asked to persist should not outlive the window that raised
/// it. Without it the server is a plain persistent `start` that runs until
/// stopped, which is what the explicit Start action wants.
fn summon(db: &Path, sock: &Path, ephemeral: bool) -> Result<(), String> {
    crate::http::summon(harbor_bin(), db, sock, &[], ephemeral)
}

#[derive(Debug, PartialEq, Eq)]
struct HttpTarget {
    host: String,
    port: u16,
}

impl HttpTarget {
    fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    fn is_local(&self) -> bool {
        self.host == "127.0.0.1"
    }
}

/// The deliberately small URL grammar Harbor speaks: plain HTTP, one host,
/// one optional port, and no path. IPv6 literals are outside DuckTable's
/// transport contract; Harbor's TCP door is IPv4-only.
fn http_target(url: &str) -> Result<HttpTarget, String> {
    if url.starts_with("https://") {
        return Err("TLS terminates in front of Harbor; use http:// or the socket".into());
    }
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("not a url: {url}"))?;
    if rest.contains(['/', '?', '#']) {
        return Err("path-prefixed HTTP targets are not supported".into());
    }
    if rest.is_empty() {
        return Err("database address has no host".into());
    }
    if rest.matches(':').count() > 1 || rest.starts_with('[') {
        return Err("IPv6 database addresses are not supported".into());
    }
    let (host, port) = match rest.rsplit_once(':') {
        Some((host, port)) => {
            let port = port.parse::<u16>().map_err(|_| format!("bad port in {url}"))?;
            if port == 0 {
                return Err("database port must be between 1 and 65535".into());
            }
            (host, port)
        }
        None => (rest, 9495),
    };
    if host.is_empty() || host.chars().any(char::is_whitespace) {
        return Err("database address has an invalid host".into());
    }
    let host = if host.eq_ignore_ascii_case("localhost") {
        "127.0.0.1".to_string()
    } else {
        host.to_string()
    };
    Ok(HttpTarget { host, port })
}

/// One app-owned OpenSSH process, forwarding a unix socket to Harbor's
/// loopback port on the host. It is shared by every clone of its Conn; the
/// last clone kills and reaps it, which binds tunnel lifetime to the
/// matching database connection without a global registry.
///
/// The socket sits in the runtime directory, which is this user's alone
/// (0700), so only this user's processes reach the database through it. A
/// loopback TCP port would be open to every process and every user on the
/// Mac, and Harbor's TCP door asks for no credential.
struct SshTunnel {
    child: Mutex<Child>,
    stderr: Arc<Mutex<Vec<u8>>>,
    host: String,
    sock: PathBuf,
}

impl SshTunnel {
    fn ensure_running(&self) -> Result<(), String> {
        let mut child = self.child.lock().map_err(|_| "SSH process lock failed")?;
        match child.try_wait().map_err(|e| format!("checking SSH to {}: {e}", self.host))? {
            None => Ok(()),
            Some(status) => Err(ssh_failure(&self.host, status.to_string(), &self.stderr)),
        }
    }
}

impl Drop for SshTunnel {
    fn drop(&mut self) {
        if let Ok(child) = self.child.get_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_file(&self.sock);
    }
}

fn ssh_failure(host: &str, status: String, stderr: &Arc<Mutex<Vec<u8>>>) -> String {
    let detail = stderr
        .lock()
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("ssh exited with {status}"));
    format!(
        "SSH connection to {host} failed: {detail}\n\nVerify the connection in Terminal:\n    ssh {host}"
    )
}

/// Where a new tunnel listens. Its name is not `*.sock`, which discovery
/// would take for a server of this machine.
fn tunnel_socket(runtime: &Path) -> Result<PathBuf, String> {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let sock = runtime.join(format!("ssh-{}-{}.tunnel", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    // sun_path holds 104 bytes with its NUL.
    if sock.as_os_str().len() > 103 {
        return Err(format!(
            "the runtime directory is too deep for a unix socket ({}): shorten $HARBOR_HOME",
            runtime.display()
        ));
    }
    Ok(sock)
}

fn ssh_command(ssh_host: &str, remote_port: u16, local: &Path) -> Command {
    // ssh splits a forward at `:` and takes `\` as an escape.
    let local = local.to_string_lossy().replace('\\', "\\\\").replace(':', "\\:");
    let mut command = Command::new("/usr/bin/ssh");
    command
        .arg("-N")
        .arg("-T")
        .arg("-S")
        .arg("none")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ExitOnForwardFailure=yes")
        .arg("-o")
        .arg("ServerAliveInterval=15")
        .arg("-o")
        .arg("ServerAliveCountMax=3")
        .arg("-o")
        .arg("TCPKeepAlive=yes")
        // A socket of the same name left by an earlier run is replaced,
        // not refused.
        .arg("-o")
        .arg("StreamLocalBindUnlink=yes")
        .arg("-L")
        .arg(format!("{local}:127.0.0.1:{remote_port}"))
        .arg(ssh_host)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    command
}

fn capture_stderr(mut pipe: impl Read + Send + 'static, captured: Arc<Mutex<Vec<u8>>>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0_u8; 1024];
        while let Ok(n) = pipe.read(&mut buf) {
            if n == 0 {
                break;
            }
            let Ok(mut out) = captured.lock() else { break };
            let room = (64_usize * 1024).saturating_sub(out.len());
            out.extend_from_slice(&buf[..n.min(room)]);
        }
    })
}

/// Start ssh and wait until its socket takes connections. A slow ProxyJump
/// chain gets twenty seconds.
fn open_tunnel(ssh_host: &str, remote_port: u16) -> Result<SshTunnel, String> {
    let runtime = runtime_dir()?;
    harbor_common::perms::ensure_private_dir(&runtime)?;
    let sock = tunnel_socket(&runtime)?;
    let mut child = ssh_command(ssh_host, remote_port, &sock)
        .spawn()
        .map_err(|e| format!("cannot run /usr/bin/ssh: {e}"))?;
    let captured = Arc::new(Mutex::new(Vec::new()));
    let reader = child.stderr.take().map(|stderr| capture_stderr(stderr, Arc::clone(&captured)));
    let tunnel = SshTunnel { child: Mutex::new(child), stderr: captured, host: ssh_host.to_string(), sock };

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let exited = tunnel.child.lock().map_err(|_| "SSH process lock failed")?.try_wait();
        if let Some(status) = exited.map_err(|e| format!("checking SSH to {ssh_host}: {e}"))? {
            // ssh is gone, so its stderr ends: read it to the end before the
            // diagnostic is built, a moment at most, since a ProxyCommand
            // ssh started may still hold the pipe.
            let read = Instant::now() + Duration::from_secs(1);
            while reader.as_ref().is_some_and(|r| !r.is_finished()) && Instant::now() < read {
                std::thread::sleep(Duration::from_millis(10));
            }
            return Err(ssh_failure(ssh_host, status.to_string(), &tunnel.stderr));
        }
        if UnixStream::connect(&tunnel.sock).is_ok() {
            return Ok(tunnel);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "SSH connection to {ssh_host} timed out\n\nVerify the connection in Terminal:\n    ssh {ssh_host}"
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Validate and persist the dialog's port-based database without opening its
/// network connection. The returned name is the normalized sidebar identity.
/// A name that already holds this address is this database saved, so a
/// second save of the same aim succeeds whichever of the two writes first;
/// a name that holds another address is refused.
pub fn add_database(name: &str, host: &str, port: &str) -> Result<String, String> {
    let name = harbor_common::normalize(name)?;
    let url = database_url(host, port)?;
    harbor_common::membership::add_remote(&name, &url).or_else(|refused| {
        if holds(&load_config()?, &name, &url) { Ok(name) } else { Err(refused) }
    })
}

/// Whether the config's remote called `name` is at the address `url` names.
fn holds(cfg: &config::FileConfig, name: &str, url: &str) -> bool {
    let saved = remote_entry(cfg, name).ok().and_then(|entry| entry.url.as_deref());
    saved.and_then(|saved| http_target(saved).ok()).is_some_and(|saved| http_target(url) == Ok(saved))
}

/// Remove only a configured remote. The kind check prevents a stale UI action
/// from ever deleting a local database's membership entry.
pub fn remove_remote(name: &str) -> Result<(), String> {
    let name = harbor_common::normalize(name)?;
    let cfg = load_config()?;
    match cfg.get(&name) {
        None => return Ok(()),
        Some(entry) if entry.kind() == config::Kind::Remote => {}
        Some(_) => return Err(format!("'{name}' is not a port-based database")),
    }
    harbor_common::membership::remove_named(&name).map(|_| ())
}

pub fn validate_database(name: &str, host: &str, port: &str) -> Result<(), String> {
    let name = harbor_common::normalize(name)?;
    database_url(host, port)?;
    if load_config()?.get(&name).is_some() {
        return Err(format!("'{name}' already exists — choose another name"));
    }
    Ok(())
}

fn database_url(host: &str, port: &str) -> Result<String, String> {
    let host = host.trim();
    if host.is_empty() {
        return Err("Host is required".into());
    }
    let port = harbor_port(port)?;
    let local = host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1";
    if !local {
        validate_ssh_host(host)?;
    }
    let host = if local { "localhost" } else { host };
    let url = format!("http://{host}:{port}");
    http_target(&url)?;
    Ok(url)
}

fn harbor_port(port: &str) -> Result<u16, String> {
    let port = port.trim();
    let parsed = port
        .parse::<u16>()
        .map_err(|_| "Port must be between 1 and 65535".to_string())?;
    if parsed == 0 {
        return Err("Port must be between 1 and 65535".into());
    }
    Ok(parsed)
}

fn validate_ssh_host(host: &str) -> Result<(), String> {
    if host.is_empty() {
        return Err("SSH host is empty".into());
    }
    if host.starts_with('-') || host.chars().any(char::is_whitespace) {
        return Err("SSH host must be a host, SSH alias, or user@host".into());
    }
    Ok(())
}

/// `GET /info` — server identity, for the inspector's Metadata section.
pub fn info(conn: &Conn) -> Result<wire::InfoResponse, String> {
    info_of(conn.transport()?, Duration::from_secs(5)).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// A server that answers one anchor's `/ready` on `sock` and waits for
    /// the anchor to let go.
    fn accepting(sock: &Path) -> std::thread::JoinHandle<()> {
        let _ = std::fs::remove_file(sock);
        let listener = std::os::unix::net::UnixListener::bind(sock).unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut byte = [0_u8; 1];
            while stream.read(&mut byte).unwrap() == 1 {
                request.push(byte[0]);
                if request.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            assert!(String::from_utf8(request).unwrap().contains("Connection: keep-alive"));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .unwrap();
            let mut byte = [0_u8; 1];
            assert_eq!(stream.read(&mut byte).unwrap(), 0);
        })
    }

    /// A socket path of this test's own, short enough for sun_path.
    fn test_socket(name: &str) -> PathBuf {
        PathBuf::from(format!("/tmp/hc-{name}-{}.sock", std::process::id()))
    }

    // The name law and the socket-naming rule live in harbor-common,
    // shared with harbor itself — no client-side copy to drift.

    #[test]
    fn a_name_holds_an_address_only_as_a_remote_at_that_host_and_port() {
        let cfg = config::parse(
            "[connection.prod]\nurl = \"http://db.example\"\n\
             [connection.dev]\nurl = \"http://localhost:9600\"\n\
             [connection.disk]\npath = \"/tmp/disk.duckdb\"\n",
        )
        .unwrap();
        // The same address, however it is spelled.
        assert!(holds(&cfg, "prod", "http://db.example:9495"));
        assert!(holds(&cfg, "dev", "http://127.0.0.1:9600"));
        // Another port or host under the name is another database.
        assert!(!holds(&cfg, "prod", "http://db.example:9496"));
        assert!(!holds(&cfg, "dev", "http://box:9600"));
        // A file or nothing under the name holds no address.
        assert!(!holds(&cfg, "disk", "http://localhost:9495"));
        assert!(!holds(&cfg, "none", "http://db.example"));
    }

    #[test]
    fn http_target_defaults_the_port_normalizes_localhost_and_refuses_tls() {
        let local = http_target("http://localhost").unwrap();
        assert_eq!(local.addr(), "127.0.0.1:9495");
        assert!(local.is_local());
        assert_eq!(http_target("http://box:9600").unwrap().addr(), "box:9600");
        assert!(http_target("https://box").is_err());
        assert!(http_target("http://box/api").is_err());
        assert!(http_target("http://[::1]:9495").is_err());
    }

    #[test]
    fn ssh_command_forwards_a_private_socket_unattended_and_owned() {
        let sock = Path::new("/Users/me/.local/state/harbor/runtime/ssh-41-0.tunnel");
        let command = ssh_command("foo.bar.com", 9494, sock);
        assert_eq!(command.get_program(), "/usr/bin/ssh");
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.windows(2).any(|a| a == ["-S", "none"]));
        assert!(args.windows(2).any(|a| a == ["-o", "BatchMode=yes"]));
        assert!(args.windows(2).any(|a| a == ["-o", "ExitOnForwardFailure=yes"]));
        assert!(args.windows(2).any(|a| a == ["-o", "ServerAliveInterval=15"]));
        assert!(args.windows(2).any(|a| a == ["-o", "ServerAliveCountMax=3"]));
        assert!(args.windows(2).any(|a| a == ["-o", "TCPKeepAlive=yes"]));
        assert!(args.windows(2).any(|a| a == ["-o", "StreamLocalBindUnlink=yes"]));
        assert!(args.windows(2).any(|a| {
            a == ["-L", "/Users/me/.local/state/harbor/runtime/ssh-41-0.tunnel:127.0.0.1:9494"]
        }));
        assert_eq!(args.last().map(String::as_str), Some("foo.bar.com"));
        // A path with ssh's separator or its escape in it stays one field.
        let odd = ssh_command("h", 9494, Path::new("/odd:dir\\x/s.tunnel"));
        assert!(odd.get_args().any(|a| a.to_string_lossy() == "/odd\\:dir\\\\x/s.tunnel:127.0.0.1:9494"));
    }

    #[test]
    fn a_tunnel_socket_is_unique_private_to_its_directory_and_fits_sun_path() {
        let runtime = Path::new("/Users/me/.local/state/harbor/runtime");
        let (a, b) = (tunnel_socket(runtime).unwrap(), tunnel_socket(runtime).unwrap());
        assert_ne!(a, b);
        assert_eq!(a.parent(), Some(runtime));
        // Not a name discovery reads as a server's.
        assert!(a.extension().is_some_and(|x| x == "tunnel"));
        let deep = PathBuf::from(format!("/{}", "d".repeat(90)));
        assert!(tunnel_socket(&deep).unwrap_err().contains("shorten $HARBOR_HOME"));
    }

    #[test]
    fn database_form_accepts_only_a_real_port_and_safe_host() {
        assert_eq!(harbor_port("9494"), Ok(9494));
        assert!(harbor_port("0").is_err());
        assert!(harbor_port("65536").is_err());
        assert!(validate_ssh_host("deploy@warehouse").is_ok());
        assert!(validate_ssh_host("").is_err());
        assert!(validate_ssh_host("-oProxyCommand=bad").is_err());
        assert!(validate_ssh_host("two hosts").is_err());
        assert_eq!(database_url("localhost", "9495").unwrap(), "http://localhost:9495");
        assert_eq!(database_url("127.0.0.1", "9495").unwrap(), "http://localhost:9495");
        assert_eq!(
            database_url("deploy@warehouse", "9494").unwrap(),
            "http://deploy@warehouse:9494"
        );
    }

    fn live(name: &str, db: &str) -> Live {
        Live {
            name: name.into(),
            db: PathBuf::from(db),
            sock: PathBuf::from(format!("/run{db}.sock")),
            version: "0.43.5".into(),
            ephemeral: false,
        }
    }

    /// The socket a file's server listens on, as `plan` is told it.
    fn sock_of(db: &Path) -> Option<PathBuf> {
        Some(PathBuf::from(format!("/run{}.sock", db.display())))
    }

    fn row(name: &str, path: Option<&str>) -> Survey {
        Survey {
            name: name.into(),
            state: State::Running,
            attached: false,
            autostart: false,
            path: path.map(PathBuf::from),
            note: None,
            size: None,
            version: None,
            ephemeral: false,
        }
    }

    #[test]
    fn a_local_database_and_a_remote_of_the_same_name_are_two_rows() {
        // The config knows `medlabs` as a remote on another host. A local
        // file of that name is being served here.
        let cfg = config::parse("[connection.medlabs]\nurl = \"http://deploy@prod:9495\"\n").unwrap();
        let servers = [live("medlabs", "/tmp/medlabs.duckdb")];
        assert_eq!(
            plan(&cfg, &servers, &sock_of),
            vec![
                // The live server is the file it serves, and the remote's
                // entry does not make it attached.
                Planned::Live { ix: 0, attached: false },
                Planned::Remote { name: "medlabs" },
            ]
        );
        // With no local server, the remote is its own row all the same.
        assert_eq!(plan(&cfg, &[], &sock_of), vec![Planned::Remote { name: "medlabs" }]);
    }

    #[test]
    fn a_config_berth_is_hidden_only_by_the_server_of_its_own_file() {
        let cfg = config::parse(
            "[connection.a]\npath = \"/data/a.duckdb\"\n[connection.warehouse]\npath = \"/data/inventory.duckdb\"\n",
        )
        .unwrap();
        // Another file with the stem `a` is running under that name: the
        // config's `a` is a different database and keeps its row.
        let other = [live("a", "/tmp/a.duckdb")];
        assert_eq!(
            plan(&cfg, &other, &sock_of),
            vec![
                Planned::Live { ix: 0, attached: false },
                Planned::Berth { name: "a", db: "/data/a.duckdb".into() },
                Planned::Berth { name: "warehouse", db: "/data/inventory.duckdb".into() },
            ]
        );
        // The config's own files running: one row each, attached, whatever
        // name the server reports and however its path is spelled.
        let mut spelled = live("warehouse", "/private/data/inventory.duckdb");
        spelled.sock = sock_of(Path::new("/data/inventory.duckdb")).unwrap();
        let own = [live("a", "/data/a.duckdb"), spelled];
        assert_eq!(
            plan(&cfg, &own, &sock_of),
            vec![Planned::Live { ix: 0, attached: true }, Planned::Live { ix: 1, attached: true }]
        );
        // A live server the config does not know is a row, unattached.
        assert_eq!(
            plan(&config::FileConfig::default(), &other, &sock_of),
            vec![Planned::Live { ix: 0, attached: false }]
        );
    }

    #[test]
    fn a_remote_row_dials_only_a_remote_entry() {
        let cfg = config::parse(
            "[connection.prod]\nurl = \"http://deploy@prod:9495\"\n[connection.a]\npath = \"/data/a.duckdb\"\n",
        )
        .unwrap();
        assert_eq!(remote_entry(&cfg, "prod").unwrap().url.as_deref(), Some("http://deploy@prod:9495"));
        // A berth of that name is a local file, and no name at all is nothing.
        assert!(remote_entry(&cfg, "a").unwrap_err().contains("no remote database named \"a\""));
        assert!(remote_entry(&cfg, "missing").is_err());
    }

    #[test]
    fn two_spellings_of_one_file_are_one_database() {
        let dir = std::env::temp_dir();
        let db = dir.join("harbor-client-a.duckdb");
        let same = dir.join(".").join("harbor-client-a.duckdb");
        let other = dir.join("harbor-client-other").join("harbor-client-a.duckdb");
        assert!(same_file(&same, &db) && !same_file(&other, &db));
    }

    #[test]
    fn a_server_that_refuses_the_anchor_says_why() {
        // Nothing listens: nothing to join, and so a server may be started.
        let db = std::env::temp_dir().join("harbor-client-join.duckdb");
        let sock = test_socket("join");
        let _ = std::fs::remove_file(&sock);
        assert!(join("join", &db, &sock, false).unwrap().is_none());

        // A server that answers `/ready` once and then refuses it, as one
        // shutting down does: its refusal is the answer, not a summon over it.
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let server = std::thread::spawn(move || {
            for reply in ["200 OK", "503 Service Unavailable"] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut byte = [0_u8; 1];
                while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap() == 1 {
                    request.push(byte[0]);
                }
                write!(stream, "HTTP/1.1 {reply}\r\nContent-Length: 0\r\n\r\n").unwrap();
            }
        });
        let refused = join("join", &db, &sock, false).err().unwrap();
        assert!(refused.contains("HTTP 503"), "{refused}");
        server.join().unwrap();
        std::fs::remove_file(&sock).unwrap();
    }

    #[test]
    fn a_stop_reaches_only_the_server_of_its_own_file() {
        // Two servers report the name `a`: two files with the same stem.
        let mine = live("a", "/data/a.duckdb");
        let other = live("a", "/tmp/a.duckdb");
        let mut renamed = live("a", "/data/a.duckdb");
        renamed.sock = PathBuf::from("/run/a.sock");
        let servers = [other, mine, renamed];
        let db = Path::new("/data/a.duckdb");
        assert_eq!(
            stop_targets(db, sock_of(db), &servers),
            vec![PathBuf::from("/run/data/a.duckdb.sock"), PathBuf::from("/run/a.sock")]
        );
        // With nothing running on the file there is still its own socket to
        // try, and never the same-named server's.
        assert_eq!(
            stop_targets(db, sock_of(db), &servers[..1]),
            vec![PathBuf::from("/run/data/a.duckdb.sock")]
        );
        assert!(stop_targets(db, None, &servers[..1]).is_empty());
    }

    #[test]
    fn rows_that_share_a_name_say_which_database_each_is() {
        let mut rows = vec![
            row("alone", Some("/tmp/alone.duckdb")),
            row("medlabs", Some("/tmp/medlabs.duckdb")),
            row("medlabs", None),
            row("zed", None),
        ];
        rows[2].note = Some("Connects over SSH to prod".into());
        let warning = name_clashes(&mut rows).unwrap();
        assert!(warning.contains("named medlabs"), "{warning}");
        assert_eq!(rows[0].note, None);
        assert!(rows[1].note.as_deref().unwrap().starts_with("The file "));
        assert!(rows[1].note.as_deref().unwrap().contains("medlabs.duckdb"));
        assert_eq!(
            rows[2].note.as_deref(),
            Some("The remote database in your config. Connects over SSH to prod")
        );
        assert_eq!(rows[3].note, None);
        // Distinct names need no word.
        let mut distinct = vec![row("a", Some("/tmp/a.duckdb")), row("b", None)];
        assert_eq!(name_clashes(&mut distinct), None);
        assert!(distinct.iter().all(|r| r.note.is_none()));
    }

    #[test]
    fn the_url_host_selects_direct_or_ssh_transport() {
        assert!(http_target("http://localhost:9495").unwrap().is_local());
        assert!(http_target("http://127.0.0.1:9495").unwrap().is_local());
        assert!(!http_target("http://foo.bar.com:9494").unwrap().is_local());
    }

    #[test]
    fn the_last_connection_clone_releases_its_harbor_presence() {
        let sock = test_socket("anchor");
        let server = accepting(&sock);
        let conn = Conn::plain("local".into(), Transport::Unix(sock.clone()), true).unwrap();
        let last = conn.clone();
        drop(conn);

        // The shared anchor still belongs to the remaining connection.
        assert!(!server.is_finished());
        drop(last);
        server.join().unwrap();
        std::fs::remove_file(&sock).unwrap();
    }

    #[test]
    fn letting_go_of_presence_never_waits_on_the_dropping_thread() {
        // An anchor whose thread is busy, as one is mid-renewal.
        let busy = crate::http::beat("test-anchor", Duration::ZERO, || {
            std::thread::sleep(Duration::from_secs(2));
            None
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let began = std::time::Instant::now();
        drop(Presence(Some(busy)));
        assert!(began.elapsed() < Duration::from_millis(500), "{:?}", began.elapsed());
    }

    #[test]
    fn the_last_connection_clone_closes_its_ssh_process() {
        let exists = |pid: &str| {
            Command::new("/bin/kill")
                .args(["-0", pid])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        let child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let pid = child.id().to_string();
        let sock = test_socket("tunnel");
        let server = accepting(&sock);
        let conn = Conn::tunneled(
            "remote".into(),
            SshTunnel {
                child: Mutex::new(child),
                stderr: Arc::new(Mutex::new(Vec::new())),
                host: "remote".into(),
                sock: sock.clone(),
            },
        )
        .unwrap();
        let last = conn.clone();
        drop(conn);
        assert!(exists(&pid) && sock.exists());
        drop(last);
        assert!(!exists(&pid));
        assert!(!sock.exists(), "the tunnel's socket goes with it");
        server.join().unwrap();
    }
}
