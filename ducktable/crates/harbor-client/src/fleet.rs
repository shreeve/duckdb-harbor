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
//! The lifecycle law is harbor's own, verbatim: **a detached start is
//! ephemeral — it lives while anyone is connected; an attached (or bare)
//! start is persistent — it lives until stopped.** DuckTable holds no anchor
//! connection (its requests are one-shot), so when it summons a database it
//! summons a persistent `start` — up until stopped — rather than a refcounted
//! one that would retire between clicks.

use crate::http::{Transport, request};
use harbor_common::State;
use harbor_common::config;
use harbor_common::paths::{self, runtime_dir};
use std::path::{Path, PathBuf};
use std::time::Duration;

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
    /// the lifecycle verbs target.
    pub path: Option<PathBuf>,
    /// A human-readable note for an unusual row (kept for future use; the
    /// socket-scan world has far fewer ways to be unhealthy than the
    /// sidecar world did).
    pub note: Option<String>,
    /// Size on disk (data file + WAL) — knowable without a connection,
    /// so stopped databases answer too.
    pub size: Option<u64>,
    /// The harbor version a running server reports (`None` when stopped or
    /// when an older server did not answer `/info`). Compared against the
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

/// Every server actually listening right now — the same discovery bare
/// `harbor` performs: `readdir` for `*.sock`, `GET /info` per socket. A
/// socket that does not answer is skipped, not unlinked: sweeping residue
/// is harbor's job, and this is a read-only view.
fn discover() -> Vec<Live> {
    let Ok(runtime) = runtime_dir() else { return Vec::new() };
    let Ok(rd) = std::fs::read_dir(&runtime) else { return Vec::new() };
    let mut out = Vec::new();
    for sock in rd.filter_map(|e| e.ok().map(|e| e.path())) {
        if !sock.extension().is_some_and(|x| x == "sock") {
            continue;
        }
        #[cfg(not(unix))]
        {
            continue;
        }
        #[cfg(unix)]
        {
            let t = Transport::Unix(sock.clone());
            let Ok(r) =
                request(&t, &wire::endpoint::INFO, None, Some(Duration::from_secs(2)))
            else {
                continue;
            };
            if r.status != 200 {
                continue;
            }
            let Ok(body) = r.body_string() else { continue };
            let Ok(info) = serde_json::from_str::<wire::InfoResponse>(body.trim()) else {
                continue;
            };
            // 0.22.1-and-earlier servers send no name (the field entered
            // /info after them) — label the row from the file stem rather
            // than showing a blank.
            let name = if info.name.is_empty() {
                std::path::Path::new(&info.database)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| info.database.clone())
            } else {
                info.name
            };
            out.push(Live {
                name,
                db: PathBuf::from(info.database),
                sock,
                version: info.harbor_version,
                ephemeral: info.ephemeral,
            });
        }
    }
    out
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

/// Every database a live socket or the config knows, sorted by name. A
/// live server's own `/info` name wins; config entries contribute the
/// stopped rows and the remotes. An unreadable config contributes a
/// warning instead of silently contributing nothing.
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
    let mut out: Vec<Survey> = live
        .iter()
        .map(|l| Survey {
            name: l.name.clone(),
            state: State::Running,
            attached: cfg.get(&l.name).is_some(),
            autostart: harbor_common::autostart::installed(&l.name),
            path: Some(l.db.clone()),
            note: None,
            size: disk_size(&l.db),
            version: Some(l.version.clone()),
            ephemeral: l.ephemeral,
        })
        .collect();

    // Config entries with a local path: a row each, unless a live server
    // already owns the name (its own /info name) or the file's socket.
    // A server /info could not identify still answers /ready — probe both
    // socket generations so a running database is never shown stopped.
    if let Ok(home) = runtime_dir() {
        for (name, entry) in cfg.berths() {
            let Some(db) = entry.database() else { continue };
            let sock21 = paths::socket_for(&home, &db).ok();
            let claimed = out.iter().any(|s| s.name == name)
                || sock21.as_ref().is_some_and(|s| live.iter().any(|l| &l.sock == s));
            if claimed {
                continue;
            }
            let running = [sock21, Some(paths::sock_file(&home, name))]
                .into_iter()
                .flatten()
                .any(|s| sock_ready(&s));
            out.push(Survey {
                name: name.to_string(),
                state: if running { State::Running } else { State::Stopped },
                attached: true,
                autostart: harbor_common::autostart::installed(name),
                path: Some(db.clone()),
                note: None,
                size: disk_size(&db),
                // A server we found only by its ready socket (not `/info`)
                // reports no version — treat it as unknown, never outdated.
                version: None,
                ephemeral: false,
            });
        }
    }

    // Remotes have no local state at all; a probe answers for them.
    for (name, entry) in cfg.remotes() {
        if out.iter().any(|s| s.name == name) {
            continue;
        }
        let transport = entry.url.as_deref().and_then(|u| url_transport(u).ok());
        let alive = transport.as_ref().is_some_and(probe);
        // Best-effort version, so the card can note a remote running behind
        // your own binary. Informational only — you cannot restart a remote
        // from here, so this never counts toward the upgrade badge.
        let version = alive
            .then(|| transport.as_ref().and_then(info_of))
            .flatten()
            .map(|i| i.harbor_version);
        out.push(Survey {
            name: name.to_string(),
            state: if alive { State::Running } else { State::Stopped },
            attached: true,
            autostart: false, // a remote has no local login item
            path: None,
            note: None,
            size: None,
            version,
            ephemeral: false,
        });
    }

    out.sort_by(|a, b| a.name.cmp(&b.name));
    Fleet { rows: out, warning }
}

/// `GET /ready` — the truth test.
fn probe(transport: &Transport) -> bool {
    request(transport, &wire::endpoint::READY, None, Some(Duration::from_millis(800)))
        .map(|r| r.status == 200)
        .unwrap_or(false)
}

/// A unix socket that exists and answers /ready.
fn sock_ready(sock: &Path) -> bool {
    #[cfg(unix)]
    {
        sock.exists() && probe(&Transport::Unix(sock.to_path_buf()))
    }
    #[cfg(not(unix))]
    {
        let _ = sock;
        false
    }
}

/// A dialable connection to one database.
#[derive(Clone)]
pub struct Conn {
    pub name: String,
    pub transport: Transport,
    /// True when this connect raised the server (worth a status line).
    /// A summoned server is an ephemeral `start` — it self-retires once its
    /// last client disconnects, so closing the window that opened it lets it
    /// go. The explicit Start action is the way to keep a server up.
    pub summoned: bool,
}

/// Resolution: config entry (url, else spawn-or-join the path's server),
/// else a live server by its own `/info` name.
pub fn connect(name: &str) -> Result<Conn, String> {
    // One name law for the whole fleet: harbor normalizes every name it
    // mints, so every lookup normalizes too.
    let name = harbor_common::normalize(name)?;
    // A bare name is a question only the config can answer — a refused
    // config must not be answered around, or a name the file defines as a
    // remote silently joins a local server that happens to share it.
    let cfg = load_config()?;

    if let Some(entry) = cfg.get(&name) {
        if entry.kind() == config::Kind::Malformed {
            return Err(format!("config entry {name:?} needs exactly one of url or path"));
        }
        if let Some(url) = &entry.url {
            return Ok(Conn { name, transport: url_transport(url)?, summoned: false });
        }
        let db = entry
            .database()
            .ok_or_else(|| format!("config entry {name:?} has neither url nor path"))?;
        let home = runtime_dir()?;
        // Join before summoning, and check both socket generations: the
        // current derived name, and the 0.19-era name-keyed socket a
        // mid-upgrade fleet still runs. Spawning over a live server would
        // only lose DuckDB's file-lock race and read as a failure.
        let sock21 = paths::socket_for(&home, &db)?;
        let sock19 = paths::sock_file(&home, &name);
        let join = |summoned: bool| {
            [&sock21, &sock19].into_iter().find(|s| sock_ready(s)).map(|s| Conn {
                name: name.clone(),
                #[cfg(unix)]
                transport: Transport::Unix(s.clone()),
                #[cfg(not(unix))]
                transport: Transport::Tcp(String::new()),
                summoned,
            })
        };
        if let Some(conn) = join(false) {
            return Ok(conn);
        }
        // Nothing serves the file yet: summon an ephemeral server — it
        // self-retires when this window's connection drops, since opening a
        // database is not a request to keep it running. Two windows can race
        // one summon; DuckDB's file lock lets exactly one server win and the
        // loser exits nonzero, so the loser judges by the end state: if a
        // socket comes ready anyway, its exit was noise.
        let spawn_err = summon(&db, true).err();
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        loop {
            if let Some(conn) = join(true) {
                return Ok(conn);
            }
            if std::time::Instant::now() > deadline {
                return Err(spawn_err
                    .unwrap_or_else(|| format!("harbor never answered for {name:?}")));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    // No config entry: a live server may still carry the name — its own
    // `/info` name, the one bare `harbor` prints.
    if let Some(l) = discover().into_iter().find(|l| l.name == name) {
        #[cfg(unix)]
        {
            return Ok(Conn {
                name,
                transport: Transport::Unix(l.sock),
                summoned: false,
            });
        }
    }
    Err(format!(
        "no running database named {name:?} — open its file with `harbor <path>` \
         or add it to the config"
    ))
}

/// Open a database FILE directly — the File→Open / drag-drop door. No
/// config consulted: the path itself is the identity. Canonicalized first,
/// so every spelling of one file meets the same server, then the named
/// flow's exact discipline: join before summoning, both socket
/// generations, and an ephemeral `start` when nothing answers.
pub fn connect_path(db: &Path) -> Result<Conn, String> {
    let db = paths::canonical_db(db).map_err(|e| format!("{}: {e}", db.display()))?;
    // The stem-derived name harbor itself would mint for this path — used
    // only for the name-keyed socket lookup; the server's /info answers
    // with its own truth on the next refresh.
    let name = db
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("no usable name in {}", db.display()))
        .and_then(harbor_common::paths::normalize)?;
    let home = runtime_dir()?;
    let sock21 = paths::socket_for(&home, &db)?;
    let sock19 = paths::sock_file(&home, &name);
    let join = |summoned: bool| {
        [&sock21, &sock19].into_iter().find(|s| sock_ready(s)).map(|s| Conn {
            name: name.clone(),
            #[cfg(unix)]
            transport: Transport::Unix(s.clone()),
            #[cfg(not(unix))]
            transport: Transport::Tcp(String::new()),
            summoned,
        })
    };
    if let Some(conn) = join(false) {
        return Ok(conn);
    }
    let spawn_err = summon(&db, true).err();
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(conn) = join(true) {
            return Ok(conn);
        }
        if std::time::Instant::now() > deadline {
            return Err(spawn_err
                .unwrap_or_else(|| format!("harbor never answered for {}", db.display())));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Stop a running server by name: POST /shutdown to its live socket, if one
/// answers. The counterpart to spawn-on-open — the GUI can now close what it
/// opened. Idempotent and never-spawning: a name with nothing running is
/// Ok(()), the same as a Stop that raced the server's own departure. Probes
/// both socket generations and any discovered server carrying the name, so a
/// mixed-era fleet stops as cleanly as a current one.
pub fn stop(name: &str) -> Result<(), String> {
    let name = harbor_common::normalize(name)?;
    let home = runtime_dir()?;
    let cfg = load_config().unwrap_or_default();

    let mut socks: Vec<PathBuf> = Vec::new();
    if let Some(db) = cfg.get(&name).and_then(|e| e.database())
        && let Ok(s) = paths::socket_for(&home, &db)
    {
        socks.push(s);
    }
    socks.push(paths::sock_file(&home, &name));
    for l in discover() {
        if l.name == name {
            socks.push(l.sock);
        }
    }

    for s in socks {
        if sock_ready(&s) {
            #[cfg(unix)]
            let t = Transport::Unix(s);
            #[cfg(not(unix))]
            let t = Transport::Tcp(String::new());
            // 202 {"stopping":true}, then the server drains and the socket
            // goes away — a refresh a beat later drops the row.
            request(&t, &wire::endpoint::SHUTDOWN, None, Some(Duration::from_secs(5)))
                .map_err(|e| format!("stop {name:?}: {e}"))?;
            return Ok(());
        }
    }
    Ok(())
}

/// Start a persistent server for this database, if one is not already up:
/// summon `harbor <db> start` and wait for its socket to answer.
pub fn start(db: &Path) -> Result<(), String> {
    let home = runtime_dir()?;
    let canon = paths::canonical_db(db)?;
    let sock = paths::socket_for(&home, &canon)?;
    if sock_ready(&sock) {
        return Ok(()); // already running
    }
    summon(&canon, false)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        if sock_ready(&sock) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err(format!("{} did not come up — see its harbor log", db.display()))
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
    let name = harbor_common::membership::name_of(db)?;
    if on {
        harbor_common::membership::attach(db)?;
        harbor_common::autostart::install(db, &name)
    } else {
        harbor_common::autostart::remove(&name).map(|_| ())
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

/// `/info` on a transport, parsed. Used for a remote's best-effort version.
fn info_of(t: &Transport) -> Option<wire::InfoResponse> {
    let r = request(t, &wire::endpoint::INFO, None, Some(Duration::from_millis(800))).ok()?;
    if r.status != 200 {
        return None;
    }
    serde_json::from_str(r.body_string().ok()?.trim()).ok()
}

/// Restart a running local server so it comes back on the current binary: stop
/// it, wait for its socket to clear (so DuckDB's file lock is released — the
/// one thing a hand-typed `stop; start` gets wrong), then summon it again in
/// the same lifetime mode. The one-click upgrade path.
pub fn restart(db: &Path, ephemeral: bool) -> Result<(), String> {
    let home = runtime_dir()?;
    let canon = paths::canonical_db(db)?;
    let sock = paths::socket_for(&home, &canon)?;
    // Shut down the server on this file's socket.
    if sock_ready(&sock) {
        #[cfg(unix)]
        let t = Transport::Unix(sock.clone());
        #[cfg(not(unix))]
        let t = Transport::Tcp(String::new());
        request(&t, &wire::endpoint::SHUTDOWN, None, Some(Duration::from_secs(5)))
            .map_err(|e| format!("stop {}: {e}", db.display()))?;
    }
    // Wait for the server to drain and release the lock.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while sock_ready(&sock) {
        if std::time::Instant::now() > deadline {
            return Err(format!("{} did not stop in time to upgrade", db.display()));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // Bring it back the way it was running.
    summon(&canon, ephemeral)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !sock_ready(&sock) {
        if std::time::Instant::now() > deadline {
            return Err(format!("{} did not come back up — see its harbor log", db.display()));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// Summon through harbor's own front door: `harbor <db> start`, detached and
/// headless. `ephemeral` sets `HARBOR_EPHEMERAL` so the child self-retires
/// once its last client disconnects — the implicit open-a-database path, where
/// a server nobody asked to persist should not outlive the window that raised
/// it. Without it the child is a plain persistent `start` that runs until
/// stopped, which is what the explicit Start action wants. Either way this
/// process only waits for the socket to answer.
fn summon(db: &Path, ephemeral: bool) -> Result<(), String> {
    let harbor = harbor_bin();
    let mut cmd = std::process::Command::new(&harbor);
    cmd.arg(db)
        .arg("start")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if ephemeral {
        cmd.env("HARBOR_EPHEMERAL", "1");
    }
    cmd.spawn()
        .map_err(|e| format!("cannot run {harbor:?} (is harbor installed?): {e}"))?;
    Ok(())
}

fn url_transport(url: &str) -> Result<Transport, String> {
    if let Some(rest) = url.strip_prefix("http://") {
        let (addr, extra) = rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p));
        if !extra.is_empty() {
            return Err("path-prefixed HTTP targets are not supported".into());
        }
        let addr = if addr.contains(':') { addr.to_string() } else { format!("{addr}:9495") };
        return Ok(Transport::Tcp(addr));
    }
    if url.starts_with("https://") {
        return Err("TLS terminates in front of Harbor; use http:// or the socket".into());
    }
    Err(format!("not a url: {url}"))
}

/// `GET /info` — server identity, for the inspector's Metadata section.
pub fn info(conn: &Conn) -> Result<wire::InfoResponse, String> {
    let r = request(
        &conn.transport,
        &wire::endpoint::INFO,
        None,
        Some(Duration::from_secs(5)),
    )
    .map_err(|e| e.to_string())?;
    let status = r.status;
    let body = r.body_string().map_err(|e| e.to_string())?;
    // Status first: an error body must not decode as an identity.
    if status != 200 {
        return Err(match wire::Event::parse(body.trim()) {
            Ok(wire::Event::Error { code, message }) => format!("{code}: {message}"),
            _ => format!("HTTP {status}"),
        });
    }
    serde_json::from_str(&body).map_err(|e| format!("bad /info response: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The name law and the socket-naming rule live in harbor-common,
    // shared with harbor itself — no client-side copy to drift.

    #[test]
    fn url_transport_defaults_the_port_and_refuses_tls() {
        assert!(matches!(url_transport("http://box").unwrap(), Transport::Tcp(a) if a == "box:9495"));
        assert!(matches!(url_transport("http://box:9600").unwrap(), Transport::Tcp(a) if a == "box:9600"));
        assert!(url_transport("https://box").is_err());
        assert!(url_transport("http://box/api").is_err());
    }
}
