//! The fleet as a GUI sees it: every berth the config or the runtime dir
//! knows, how to dial it, and what state it is honestly in.
//!
//! State truth comes from [`harbor_common::fleet::reconcile`], shared with
//! `harbor show` and `pilot` so the three views can never disagree about
//! whether a berth is running. Liveness is flock-backed (a held lock is
//! proof of life without a round trip); the probe only dials the one row
//! shape a lock cannot settle. This file layers on what only this client
//! wants: the remotes reconcile excludes by design, size on disk, and the
//! whole connection half (Conn, connect). Harbor 0.20 removed the
//! `/keepalive` route with the idle-exit machinery it served: a held
//! connection is presence now, so there is nothing to pulse.
//!
//! The lifecycle law is pilot's, verbatim: a name is a service — it starts
//! on use through harbor's own verb, which applies the whole config entry,
//! and it runs until the operator says stop. A held name (`harbor stop`)
//! refuses to rise from here; only `harbor start` lifts the hold.

use crate::http::{Transport, request};
use crate::tokens;
use harbor_common::State;
use harbor_common::config;
use harbor_common::fleet::{Addr, Sidecar, dial_host};
use harbor_common::paths::{self, runtime_dir};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// One sidebar row: reconcile's truth about a berth, plus the size on
/// disk only a GUI wants.
#[derive(Debug, Clone)]
pub struct Survey {
    pub name: String,
    pub state: State,
    /// A human-readable fix for an unhealthy row, verbatim from
    /// reconcile ("… harbor forget x").
    pub note: Option<String>,
    /// Size on disk (data file + WAL) — knowable without a connection,
    /// so stopped berths answer too.
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

fn berth_token(home: &Path, name: &str) -> Option<String> {
    let t = std::fs::read_to_string(paths::token_file(home, name)).ok()?;
    let t = t.trim().to_string();
    if t.is_empty() { None } else { Some(t) }
}

/// Where a live berth answers: its sidecar's word, never a guessed path.
/// A berth started with an explicit `--socket` answers where it said it
/// would, not where the name implies.
fn berth_transport(home: &Path, name: &str) -> Option<Transport> {
    addr_transport(&Sidecar::read(home, name)?.addr()?)
}

/// A recorded address -> how this process dials it. The socket must
/// actually be there: a sidecar can outlive its berth, and dialing residue
/// helps no one.
fn addr_transport(addr: &Addr) -> Option<Transport> {
    match addr {
        #[cfg(unix)]
        Addr::Sock(p) => p.exists().then(|| Transport::Unix(p.clone())),
        #[cfg(not(unix))]
        Addr::Sock(_) => None,
        Addr::Tcp(host, port) => Some(Transport::Tcp(format!("{}:{port}", dial_host(host)))),
    }
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

/// Every berth the config or the runtime dir knows, sorted by name, with
/// reconcile's flock-backed state. An unreadable config contributes a
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

    let mut out: Vec<Survey> = Vec::new();
    if let Ok(home) = runtime_dir() {
        // Re-scan for the db PATHS: reconcile's Row carries the display
        // (shortened) path, and the size wants the real one.
        let (sidecars, _, _) = harbor_common::fleet::scan_runtime(&home);
        let dial = |addr: &Addr| addr_transport(addr).is_some_and(|t| probe(&t));
        for r in harbor_common::fleet::reconcile(&cfg, &home, &dial) {
            let db = sidecars
                .get(&r.name)
                .and_then(|s| s.db.clone())
                .map(PathBuf::from)
                .or_else(|| cfg.get(&r.name).and_then(|c| c.database()));
            out.push(Survey {
                size: db.and_then(|p| disk_size(&p)),
                name: r.name,
                state: r.state,
                note: r.note,
            });
        }
    }

    // Remotes are excluded from reconcile by design (they have no local
    // runtime state to reconcile); a probe answers for them.
    for (name, entry) in cfg.remotes() {
        if out.iter().any(|s| s.name == name) {
            continue;
        }
        let live = entry
            .url
            .as_deref()
            .and_then(|u| url_transport(u).ok())
            .is_some_and(|t| probe(&t));
        out.push(Survey {
            name: name.to_string(),
            state: if live { State::Running } else { State::Stopped },
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

/// A dialable, authenticated connection to one berth.
#[derive(Clone)]
pub struct Conn {
    pub name: String,
    pub transport: Transport,
    pub token: Option<String>,
    /// True when this connect raised the service (worth a status line).
    /// Its lifetime is harbor's business either way: a name runs until the
    /// operator says stop, not until a window closes.
    pub summoned: bool,
}

/// Resolution follows pilot: config entry (url, else the service by name),
/// else a live berth by name. `HARBOR_TOKEN` beats the entry's own token
/// sources; a local berth falls back to its runtime token file.
pub fn connect(name: &str) -> Result<Conn, String> {
    let env_token = std::env::var("HARBOR_TOKEN").ok();
    // One name law for the whole fleet: harbor normalizes every name it
    // mints, so every lookup normalizes too.
    let name = harbor_common::normalize(name)?;
    // A bare name is a question only the config can answer — a refused
    // config must not be answered around, or a name the file defines as a
    // remote silently joins a local berth that happens to share it.
    let cfg = load_config()?;

    if let Some(entry) = cfg.get(&name) {
        if entry.kind() == config::Kind::Malformed {
            return Err(format!("config entry {name:?} needs exactly one of url or path"));
        }
        let token = env_token.clone().or_else(|| tokens::resolve(entry));
        if let Some(url) = &entry.url {
            return Ok(Conn { name, transport: url_transport(url)?, token, summoned: false });
        }
        // A name is a service: it starts on use through `harbor start
        // <name>`, so the whole entry — lifetime, limits, init SQL — is
        // harbor's to apply, and this client can never turn a persistent
        // service into a temp. The operator's stop outranks a click.
        let home = runtime_dir()?;
        if paths::hold_file(&home, &name).exists() {
            return Err(format!(
                "{name:?} is stopped by hand — harbor start {name} brings it back"
            ));
        }
        let summoned = berth_transport(&home, &name).is_none();
        if summoned
            && let Err(e) = harbor_start(&name)
        {
            // Two windows can race one summon; the flock lets exactly one
            // serve win and the other's child exits nonzero. start names an
            // end state, so the loser judges by the end state: if the name
            // comes ready anyway, its exit code was noise.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while berth_transport(&home, &name).is_none() {
                if std::time::Instant::now() > deadline {
                    return Err(e);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        let transport = berth_transport(&home, &name)
            .ok_or_else(|| format!("harbor started {name:?} but it never registered"))?;
        return Ok(Conn {
            transport,
            token: token.or_else(|| berth_token(&home, &name)),
            name,
            summoned,
        });
    }

    let home = runtime_dir()?;
    if let Some(transport) = berth_transport(&home, &name) {
        return Ok(Conn {
            transport,
            token: env_token.or_else(|| berth_token(&home, &name)),
            name,
            summoned: false,
        });
    }
    Err(format!("no running database named {name:?}"))
}

/// The one fleet-touching act, through harbor's own verb — the binary that
/// owns the rules. stdout is harbor's fleet table; a GUI has no use for it,
/// and failures still speak on stderr.
fn harbor_start(name: &str) -> Result<(), String> {
    let harbor = std::env::var("HARBOR_BIN").unwrap_or_else(|_| "harbor".to_string());
    let status = std::process::Command::new(&harbor)
        .args(["start", name])
        .stdout(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("cannot run {harbor:?} (is harbor installed?): {e}"))?;
    match status.success() {
        true => Ok(()),
        false => Err(format!("harbor start {name} failed")),
    }
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

/// `GET /info` — berth identity, for the inspector's Metadata section.
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

    // The state table, the name law, and the sidecar reader all live in
    // harbor-common, shared with `harbor show` and `pilot` — no client-side
    // copy to drift.

    #[test]
    fn url_transport_defaults_the_port_and_refuses_tls() {
        assert!(matches!(url_transport("http://box").unwrap(), Transport::Tcp(a) if a == "box:9495"));
        assert!(matches!(url_transport("http://box:9600").unwrap(), Transport::Tcp(a) if a == "box:9600"));
        assert!(url_transport("https://box").is_err());
        assert!(url_transport("http://box/api").is_err());
    }
}
