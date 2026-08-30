//! The fleet as a GUI sees it: every berth the config or the runtime dir
//! knows, how to dial it, and what state it is honestly in.
//!
//! State derivation is client-side and probe-backed: a sidecar json is a
//! claim, `GET /ready` is the truth. The mapping onto
//! [`harbor_common::State`] follows the vocabulary's own definitions —
//! Running only when configured, live, and serving the database the config
//! names; Drifted when live but disagreeing with the file; Unmanaged when
//! live with no entry; Dead when the registry claims a process the probe
//! cannot find; Stopped and Stale for the quiet cases.

use crate::http::{request, Transport};
use crate::tokens;
use harbor_common::config::{self, Connection};
use harbor_common::paths::runtime_dir;
use harbor_common::State;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// What a berth's sidecar json registered at start.
#[derive(Debug, Clone, Default)]
pub struct Sidecar {
    pub db: Option<PathBuf>,
    pub port: Option<u16>,
    pub bind: Option<String>,
}

/// One row of the fleet view: a name, what the config says about it, and
/// what the runtime dir says about it. Either half may be absent; a row
/// exists because at least one of them mentions the name.
#[derive(Debug, Clone)]
pub struct BerthRow {
    pub name: String,
    pub configured: Option<Connection>,
    pub sidecar: Option<Sidecar>,
    pub transport: Option<Transport>,
}

impl BerthRow {
    /// Spawn-on-demand candidate: configured with a path and not running.
    pub fn summonable(&self) -> bool {
        self.transport.is_none()
            && self.configured.as_ref().is_some_and(|c| c.database().is_some())
    }

    /// The database's size on disk (data file + WAL), from whichever half
    /// names the path. Needs no connection, so it works for stopped
    /// berths too.
    pub fn size_on_disk(&self) -> Option<u64> {
        let db = self
            .sidecar
            .as_ref()
            .and_then(|s| s.db.clone())
            .or_else(|| self.configured.as_ref().and_then(|c| c.database()))?;
        let main = std::fs::metadata(&db).ok()?.len();
        let mut wal = db.into_os_string();
        wal.push(".wal");
        Some(main + std::fs::metadata(wal).map(|m| m.len()).unwrap_or(0))
    }
}

fn berth_sock(home: &Path, name: &str) -> PathBuf {
    home.join(format!("{name}.sock"))
}

fn berth_token(home: &Path, name: &str) -> Option<String> {
    let t = std::fs::read_to_string(home.join(format!("{name}.token"))).ok()?;
    let t = t.trim().to_string();
    if t.is_empty() { None } else { Some(t) }
}

fn read_sidecar(home: &Path, name: &str) -> Option<Sidecar> {
    let text = std::fs::read_to_string(home.join(format!("{name}.json"))).ok()?;
    let j: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(Sidecar {
        db: j["db"].as_str().map(PathBuf::from),
        port: j["port"].as_u64().map(|p| p as u16),
        bind: j["bind"].as_str().map(str::to_string),
    })
}

/// How to dial a live local berth: the socket, else the TCP address its
/// sidecar registered (loopback when it bound every interface).
fn berth_transport(home: &Path, name: &str, sidecar: Option<&Sidecar>) -> Option<Transport> {
    #[cfg(unix)]
    {
        let sock = berth_sock(home, name);
        if sock.exists() {
            return Some(Transport::Unix(sock));
        }
    }
    let sc = sidecar?;
    let port = sc.port?;
    let bind = match sc.bind.as_deref() {
        Some("0.0.0.0") | Some("::") | None => "127.0.0.1",
        Some(b) => b,
    };
    Some(Transport::Tcp(format!("{bind}:{port}")))
}

/// Every berth the config or the runtime dir knows, sorted by name. A
/// missing config file or an unreachable runtime dir contributes nothing
/// rather than failing the view; the GUI's empty state says where the
/// config lives.
pub fn list() -> Vec<BerthRow> {
    let cfg = config::load_or_empty("ducktable");
    let home = runtime_dir().ok();

    let mut names: Vec<String> = cfg.berths().iter().map(|(n, _)| n.to_string()).collect();
    names.extend(cfg.remotes().iter().map(|(n, _)| n.to_string()));
    if let Some(home) = &home {
        if let Ok(rd) = std::fs::read_dir(home) {
            for e in rd.filter_map(|e| e.ok()) {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "json") {
                    if let Some(stem) = p.file_stem() {
                        names.push(stem.to_string_lossy().into_owned());
                    }
                }
            }
        }
    }
    names.sort();
    names.dedup();

    names
        .into_iter()
        .map(|name| {
            let sidecar = home.as_deref().and_then(|h| read_sidecar(h, &name));
            let transport =
                home.as_deref().and_then(|h| berth_transport(h, &name, sidecar.as_ref()));
            BerthRow {
                configured: cfg.get(&name).cloned(),
                sidecar,
                transport,
                name,
            }
        })
        .collect()
}

/// `GET /ready` — the only unauthenticated route, and the truth test.
pub fn probe(transport: &Transport) -> bool {
    request(transport, &wire::endpoint::READY, None, None, Some(Duration::from_millis(800)))
        .map(|r| r.status == 200)
        .unwrap_or(false)
}

/// The honest state of a row given the probe's answer (None when there was
/// nothing to dial).
pub fn state_of(row: &BerthRow, live: Option<bool>) -> State {
    match (&row.configured, &row.transport, live) {
        (_, Some(_), Some(false)) => State::Dead,
        (Some(c), Some(_), Some(true)) => {
            let entry_db = c.database().map(canonical);
            let live_db = row.sidecar.as_ref().and_then(|s| s.db.clone()).map(canonical);
            match (entry_db, live_db) {
                (Some(a), Some(b)) if a != b => State::Drifted,
                _ => State::Running,
            }
        }
        (None, Some(_), Some(true)) => State::Unmanaged,
        (Some(_), None, _) => State::Stopped,
        (None, None, _) => State::Stale,
        (_, Some(_), None) => State::Dead,
    }
}

fn canonical(p: PathBuf) -> PathBuf {
    std::fs::canonicalize(&p).unwrap_or(p)
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
    let sidecar = read_sidecar(&home, name);
    if let Some(transport) = berth_transport(&home, name, sidecar.as_ref()) {
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
                    let sc = read_sidecar(&home, name);
                    if let Some(t) = berth_transport(&home, name, sc.as_ref()) {
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
    let sc = read_sidecar(&home, &name);
    let transport = berth_transport(&home, &name, sc.as_ref())
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

    fn row(configured: bool, transport: bool, db: (&str, &str)) -> BerthRow {
        let cfg: harbor_common::config::FileConfig = toml::from_str(&format!(
            "[connection.x]\npath = \"{}\"\n",
            db.0
        ))
        .unwrap();
        BerthRow {
            name: "x".into(),
            configured: configured.then(|| cfg.connection["x"].clone()),
            sidecar: Some(Sidecar { db: Some(PathBuf::from(db.1)), ..Default::default() }),
            transport: transport.then(|| Transport::Tcp("127.0.0.1:1".into())),
        }
    }

    #[test]
    fn the_state_table() {
        let same = ("/tmp/a.duckdb", "/tmp/a.duckdb");
        let differ = ("/tmp/a.duckdb", "/tmp/b.duckdb");
        assert_eq!(state_of(&row(true, true, same), Some(true)), State::Running);
        assert_eq!(state_of(&row(true, true, differ), Some(true)), State::Drifted);
        assert_eq!(state_of(&row(false, true, same), Some(true)), State::Unmanaged);
        assert_eq!(state_of(&row(true, true, same), Some(false)), State::Dead);
        assert_eq!(state_of(&row(true, false, same), None), State::Stopped);
        assert_eq!(state_of(&row(false, false, same), None), State::Stale);
    }

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
