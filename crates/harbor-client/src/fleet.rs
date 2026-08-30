//! The fleet as a GUI sees it: every berth the config or the runtime dir
//! knows, how to dial it, and what state it is honestly in.
//!
//! State truth comes from [`harbor_common::fleet::reconcile`], shared with
//! `harbor show` and `pilot` so the three views can never disagree about
//! whether a berth is running. Liveness is flock-backed (a held lock is
//! proof of life without a round trip); the probe only dials the one row
//! shape a lock cannot settle. This file layers on what only this client
//! wants: the remotes reconcile excludes by design, size on disk, and the
//! whole connection half (Conn, connect, keepalive).

use crate::http::{request, Transport};
use crate::tokens;
use harbor_common::config;
use harbor_common::fleet::Addr;
use harbor_common::paths::runtime_dir;
use harbor_common::State;
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

/// db file + its `.wal`, when the file exists.
fn disk_size(db: &Path) -> Option<u64> {
    let main = std::fs::metadata(db).ok()?.len();
    let mut wal = db.as_os_str().to_owned();
    wal.push(".wal");
    Some(main + std::fs::metadata(wal).map(|m| m.len()).unwrap_or(0))
}

fn berth_sock(home: &Path, name: &str) -> PathBuf {
    home.join(format!("{name}.sock"))
}

fn berth_token(home: &Path, name: &str) -> Option<String> {
    let t = std::fs::read_to_string(home.join(format!("{name}.token"))).ok()?;
    let t = t.trim().to_string();
    if t.is_empty() { None } else { Some(t) }
}

/// How to dial a live local berth: whatever its sidecar says it bound.
///
/// Read, not derived. This used to guess `<runtime>/<name>.sock` and fall
/// back to the port — which is right only while the socket happens to sit at
/// the path the name implies. A berth started with an explicit `--socket`
/// answers somewhere else entirely, and the guess would then dial a path that
/// is not there and report a live berth as Dead. The berth records where it
/// actually bound; that is the answer.
fn read_transport(home: &Path, name: &str) -> Option<Transport> {
    let text = std::fs::read_to_string(home.join(format!("{name}.json"))).ok()?;
    let j: serde_json::Value = serde_json::from_str(&text).ok()?;
    addr_transport(&Addr::read(&j)?)
}

/// Where the berth recorded it bound -> how this process dials it.
fn addr_transport(addr: &Addr) -> Option<Transport> {
    match addr {
        #[cfg(unix)]
        Addr::Sock(p) => p.exists().then(|| Transport::Unix(p.clone())),
        #[cfg(not(unix))]
        Addr::Sock(_) => None,
        Addr::Tcp(host, port) => {
            let bind = match host.as_str() {
                "0.0.0.0" | "::" => "127.0.0.1",
                other => other,
            };
            Some(Transport::Tcp(format!("{bind}:{port}")))
        }
    }
}

/// Every berth the config or the runtime dir knows, sorted by name,
/// with reconcile's flock-backed state. A missing config file or an
/// unreachable runtime dir contributes nothing rather than failing the
/// view; the GUI's empty state says where the config lives.
pub fn survey() -> Vec<Survey> {
    let cfg = config::load_or_empty("ducktable");
    let mut out: Vec<Survey> = Vec::new();

    if let Ok(home) = runtime_dir() {
        // Re-scan for the db PATHS: reconcile's Row carries the display
        // (shortened) path, and the size wants the real one.
        let (sidecars, _) = harbor_common::fleet::scan_runtime(&home);
        let dial = |addr: &Addr| addr_transport(addr).is_some_and(|t| probe(&t));
        for r in harbor_common::fleet::reconcile(&cfg, &home, &dial) {
            let db = sidecars
                .get(&r.name)
                .and_then(|j| j["db"].as_str())
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
    out
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
    /// True when this connect summoned the berth: the lifetime is ours, and
    /// closing the last window over it lets it retire. Joining a running
    /// berth obliges nothing.
    pub summoned: bool,
}

/// Resolution follows pilot: config entry (url, else path via join-or-
/// summon), else a live berth by name. `HARBOR_TOKEN` beats the entry's own
/// token sources; a summoned or live local berth falls back to its runtime
/// token file.
pub fn connect(name: &str) -> Result<Conn, String> {
    let env_token = std::env::var("HARBOR_TOKEN").ok();
    let cfg = config::load_or_empty("ducktable");

    if let Some(entry) = cfg.get(name) {
        let token = env_token.clone().or_else(|| tokens::resolve(entry));
        if let Some(url) = &entry.url {
            return Ok(Conn {
                name: name.to_string(),
                transport: url_transport(url)?,
                token,
                summoned: false,
            });
        }
        if let Some(db) = entry.database() {
            let idle = entry.idle_exit.as_deref().unwrap_or("90s");
            let (transport, file_token, summoned) = ensure_berth(&db, idle)?;
            return Ok(Conn {
                name: name.to_string(),
                transport,
                token: token.or(file_token),
                summoned,
            });
        }
        return Err(format!("config entry {name:?} has neither url nor path"));
    }

    let home = runtime_dir()?;
    if let Some(transport) = read_transport(&home, name) {
        return Ok(Conn {
            name: name.to_string(),
            transport,
            token: env_token.or_else(|| berth_token(&home, name)),
            summoned: false,
        });
    }
    Err(format!("no running database named {name:?}"))
}

/// Join the berth serving this file, else summon one via `harbor start`
/// (named `harbor add` before 0.15.0) — pilot's D9 semantics, including
/// the name-collision guard that keeps a summon from silently querying
/// the wrong database.
fn ensure_berth(
    path: &Path,
    idle_exit: &str,
) -> Result<(Transport, Option<String>, bool), String> {
    let home = runtime_dir()?;
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

    if let Ok(rd) = std::fs::read_dir(&home) {
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            if p.extension().is_none_or(|x| x != "json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&p) else { continue };
            let Ok(j) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
            if j["db"].as_str() == Some(&canon.display().to_string()) {
                if let Some(name) = j["name"].as_str() {
                    if let Some(t) = read_transport(&home, name) {
                        return Ok((t, berth_token(&home, name), false));
                    }
                }
            }
        }
    }

    let name = derived_name(&canon)
        .ok_or_else(|| format!("cannot derive a database name from {}", canon.display()))?;
    let sidecar = home.join(format!("{name}.json"));
    if berth_sock(&home, &name).exists() || sidecar.exists() {
        let other = std::fs::read_to_string(&sidecar)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|j| j["db"].as_str().map(str::to_string))
            .unwrap_or_else(|| "another database".to_string());
        return Err(format!(
            "{name:?} is already running but serves {other}, not {}",
            canon.display()
        ));
    }
    let harbor = std::env::var("HARBOR_BIN").unwrap_or_else(|_| "harbor".to_string());
    let status = std::process::Command::new(&harbor)
        .arg("start")
        .arg(&canon)
        .args(["--name", &name, "--idle-exit", idle_exit])
        .status()
        .map_err(|e| format!("cannot run {harbor:?} (is harbor installed?): {e}"))?;
    if !status.success() {
        return Err(format!("harbor start failed for {}", canon.display()));
    }
    let transport = read_transport(&home, &name)
        .ok_or_else(|| format!("harbor start returned without registering {name:?}"))?;
    Ok((transport, berth_token(&home, &name), true))
}

fn derived_name(path: &Path) -> Option<String> {
    let name: String = path
        .file_stem()?
        .to_string_lossy()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '-' })
        .collect();
    // A leading '-' (e.g. from "/data/ my.duckdb") would read as a flag
    // when passed to `harbor start --name`.
    let name = name.trim_start_matches('-').to_string();
    if name.is_empty() { None } else { Some(name) }
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
    let body = r.body_string().map_err(|e| e.to_string())?;
    if let Ok(info) = serde_json::from_str::<wire::InfoResponse>(&body) {
        return Ok(info);
    }
    match wire::Event::parse(body.trim()) {
        Ok(wire::Event::Error { code, message }) => Err(format!("{code}: {message}")),
        _ => Err(format!("unexpected /info response: {}", body.chars().take(120).collect::<String>())),
    }
}

/// `GET /keepalive` — resets the berth's idle clock. Pulsed while a window
/// holds a connection, so an idle-exit berth never retires under an open
/// grid.
pub fn keepalive(conn: &Conn) -> bool {
    request(
        &conn.transport,
        &wire::endpoint::KEEPALIVE,
        conn.token.as_deref(),
        None,
        Some(Duration::from_secs(3)),
    )
    .map(|r| r.status == 200)
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The state table itself lives in harbor_common::fleet::reconcile,
    // shared with `harbor show` and `pilot` — no client-side copy to
    // drift.

    #[test]
    fn derived_names_are_filesystem_tame() {
        assert_eq!(derived_name(Path::new("/a/My Data.duckdb")), Some("my-data".into()));
        assert_eq!(derived_name(Path::new("/a/medlabs.duckdb")), Some("medlabs".into()));
        assert_eq!(derived_name(Path::new("/a/ my.duckdb")), Some("my".into()));
        assert_eq!(derived_name(Path::new("/a/---.duckdb")), None);
    }

    #[test]
    fn url_transport_defaults_the_port_and_refuses_tls() {
        assert!(matches!(url_transport("http://box").unwrap(), Transport::Tcp(a) if a == "box:9495"));
        assert!(matches!(url_transport("http://box:9600").unwrap(), Transport::Tcp(a) if a == "box:9600"));
        assert!(url_transport("https://box").is_err());
        assert!(url_transport("http://box/api").is_err());
    }
}
