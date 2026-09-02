//! The fleet as a GUI sees it: every database the config or a live socket
//! knows, how to dial it, and what state it is honestly in.
//!
//! Truth comes the same way `harbor` (bare) finds it: the runtime dir is
//! scanned for `*.sock` and each socket answers `GET /info` — the listening
//! socket IS the registration, there is no sidecar, lock file, or registry
//! to read. Harbor 0.20 removed all three along with the `/keepalive` route
//! and its idle-exit machinery: a held connection is presence now, so there
//! is nothing to pulse and nothing to reconcile. This file layers on what
//! only this client wants: config-named remotes, size on disk, and the
//! whole connection half (Conn, connect).
//!
//! The lifecycle law is harbor's own, verbatim: **bare, the server is
//! everyone's — it lives while anyone is connected; `serve`, the server is
//! yours — it lives until you leave.** DuckTable holds no anchor connection
//! (its requests are one-shot), so when it summons a database it summons a
//! `serve` — an owned server that stays up until stopped — rather than a
//! refcounted one that would retire between clicks.

use crate::http::{Transport, request};
use crate::tokens;
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
    /// A human-readable note for an unusual row (kept for future use; the
    /// socket-scan world has far fewer ways to be unhealthy than the
    /// sidecar world did).
    pub note: Option<String>,
    /// Size on disk (data file + WAL) — knowable without a connection,
    /// so stopped databases answer too.
    pub size: Option<u64>,
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
}

/// A 0.19-era runtime token, when one exists beside the socket. Current
/// harbors put no token on a unix socket (the 0700 dir is the access
/// control), but a fleet mid-upgrade still runs older servers that demand
/// theirs — and a mixed fleet is the normal state of a real machine.
fn berth_token(home: &Path, name: &str) -> Option<String> {
    let t = std::fs::read_to_string(paths::token_file(home, name)).ok()?;
    let t = t.trim().to_string();
    if t.is_empty() { None } else { Some(t) }
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
            let ask = |token: Option<&str>| {
                request(&t, &wire::endpoint::INFO, token, None, Some(Duration::from_secs(2))).ok()
            };
            let mut r = match ask(None) {
                Some(r) => r,
                None => continue,
            };
            // An older server may guard /info with its runtime token; the
            // token file sits beside the socket, named after it.
            if r.status == 401
                && let Some(stem) = sock.file_stem().and_then(|s| s.to_str())
                && let Some(tok) = sock.parent().and_then(|d| berth_token(d, stem))
                && let Some(retry) = ask(Some(&tok))
            {
                r = retry;
            }
            if r.status != 200 {
                continue;
            }
            let Ok(body) = r.body_string() else { continue };
            let Ok(info) = serde_json::from_str::<wire::InfoResponse>(body.trim()) else {
                continue;
            };
            out.push(Live { name: info.name, db: PathBuf::from(info.database), sock });
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
            note: None,
            size: disk_size(&l.db),
        })
        .collect();

    // Config entries with a local path: a row each, unless a live server
    // already owns the name (its own /info name) or the file's socket.
    // A server /info could not identify (an older one with no readable
    // token) still answers /ready — probe both socket generations so a
    // running database is never shown stopped.
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
                note: None,
                size: disk_size(&db),
            });
        }
    }

    // Remotes have no local state at all; a probe answers for them.
    for (name, entry) in cfg.remotes() {
        if out.iter().any(|s| s.name == name) {
            continue;
        }
        let alive = entry
            .url
            .as_deref()
            .and_then(|u| url_transport(u).ok())
            .is_some_and(|t| probe(&t));
        out.push(Survey {
            name: name.to_string(),
            state: if alive { State::Running } else { State::Stopped },
            note: None,
            size: None,
        });
    }

    out.sort_by(|a, b| a.name.cmp(&b.name));
    Fleet { rows: out, warning }
}

/// `GET /ready` — the only unauthenticated route, and the truth test.
fn probe(transport: &Transport) -> bool {
    request(transport, &wire::endpoint::READY, None, None, Some(Duration::from_millis(800)))
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

/// A dialable, authenticated connection to one database.
#[derive(Clone)]
pub struct Conn {
    pub name: String,
    pub transport: Transport,
    pub token: Option<String>,
    /// True when this connect raised the server (worth a status line).
    /// A summoned server is a `serve` — owned, it runs until stopped, not
    /// until a window closes.
    pub summoned: bool,
}

/// Resolution: config entry (url, else spawn-or-join the path's server),
/// else a live server by its own `/info` name. `HARBOR_TOKEN` beats the
/// entry's own token sources; a unix socket needs no token at all.
pub fn connect(name: &str) -> Result<Conn, String> {
    let env_token = std::env::var("HARBOR_TOKEN").ok();
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
        let token = env_token.clone().or_else(|| tokens::resolve(entry));
        if let Some(url) = &entry.url {
            return Ok(Conn { name, transport: url_transport(url)?, token, summoned: false });
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
                token: token.clone().or_else(|| berth_token(&home, &name)),
                summoned,
            })
        };
        if let Some(conn) = join(false) {
            return Ok(conn);
        }
        // Nothing serves the file yet: summon a `serve` — harbor's own
        // owned lifetime, up until the operator stops it. Two windows can
        // race one summon; DuckDB's file lock lets exactly one server win
        // and the loser exits nonzero, so the loser judges by the end
        // state: if a socket comes ready anyway, its exit was noise.
        let spawn_err = harbor_serve(&db).err();
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
                token: env_token,
                summoned: false,
            });
        }
    }
    Err(format!(
        "no running database named {name:?} — open its file with `harbor <path>` \
         or add it to the config"
    ))
}

/// Summon through harbor's own front door: `harbor <db> serve`, detached and
/// headless. The child owns its lifetime; this process only waits for the
/// socket to answer.
fn harbor_serve(db: &Path) -> Result<(), String> {
    let harbor = std::env::var("HARBOR_BIN").unwrap_or_else(|_| "harbor".to_string());
    std::process::Command::new(&harbor)
        .arg(db)
        .arg("serve")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
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
        conn.token.as_deref(),
        None,
        Some(Duration::from_secs(5)),
    )
    .map_err(|e| e.to_string())?;
    let status = r.status;
    let body = r.body_string().map_err(|e| e.to_string())?;
    // Status first: a 401's error body must not decode as an identity.
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
