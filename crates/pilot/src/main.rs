//! pilot — the Harbor client.
//!
//! Zero-config local (D10a): a bare name resolves to ~/.config/harbor/runtime/<name>.sock —
//! or, for a --port berth, the TCP address its sidecar json registered — plus
//! its token file; config.toml is purely additive (remotes, aliases, taste).
//! TLS is Caddy's job (D6) — pilot speaks plain HTTP over UDS/TCP.
//!
//!   pilot                          list live berths
//!   pilot <target>                 the REPL
//!   pilot <target> -c "SQL"        run one statement
//!   echo "SQL" | pilot <target>    same, from stdin
//!
//! <target> = config entry | berth name | file.duckdb | socket | http://host:port

mod complete;
mod config;
mod render;
mod highlight;
mod http;
mod keywords;
mod repl;
mod scan;
mod theme;

use wire::{Event, SqlRequest, endpoint};
use render::{Mode, RenderOpts, Renderer};
use http::Transport;
use std::io::{BufRead, IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// Set by SIGINT while a statement streams (registered in repl::run);
/// checked at every read tick. Cleared before each statement.
static CANCEL: LazyLock<std::sync::Arc<AtomicBool>> =
    LazyLock::new(|| std::sync::Arc::new(AtomicBool::new(false)));
static QUERY_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
struct Conn {
    transport: Transport,
    token: Option<String>,
}

/// What became of one statement. The REPL and `.read` stop a multi-statement
/// run on anything but Done; main() maps it to the process exit code.
#[derive(Clone, Copy, PartialEq)]
enum Outcome {
    Done,
    Cancelled,
    Failed,
}

fn main() -> ExitCode {
    // Ctrl-C cancels the running statement (via its queryId), it does not
    // kill pilot. At the REPL prompt reedline runs raw mode, so SIGINT only
    // fires while a statement streams; a second Ctrl-C while the first
    // cancel is still pending exits outright (the tick handler enforces it).
    let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, CANCEL.clone());
    let mut args = std::env::args().skip(1);
    let mut target: Option<String> = None;
    let mut sql: Option<String> = None;
    let mut token: Option<String> = None;
    let mut json = false;
    let mut mode: Option<String> = None;

    while let Some(a) = args.next() {
        match a.as_str() {
            "-c" | "--command" => match args.next() {
                Some(v) => sql = Some(v),
                None => return fail("-c needs the SQL to run"),
            },
            "--token" => match args.next() {
                Some(v) => token = Some(v),
                None => return fail("--token needs a value"),
            },
            "--json" => json = true,
            "--mode" => match args.next() {
                Some(v) => mode = Some(v),
                None => return fail("--mode needs a mode name"),
            },
            "-h" | "--help" => {
                print!("{HELP}");
                return ExitCode::SUCCESS;
            }
            "-V" | "--version" | "version" => {
                println!("pilot {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            _ if target.is_none() => target = Some(a),
            _ => return fail(&format!("unexpected argument: {a}")),
        }
    }

    let Some(target) = target else { return list_fleet() };

    let cfg = config::load(); // once; resolve and the render defaults share it
    let conn = match resolve(&cfg, &target, token) {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };

    // config [defaults], then the flags on top — for the REPL and one-shots.
    let mut opts = RenderOpts::with_defaults(&cfg.defaults);
    if json {
        opts.mode = Mode::JsonLines;
    }
    if let Some(m) = mode {
        match Mode::parse(&m) {
            Some(m) => opts.mode = m,
            None => return fail(&format!("unknown mode {m:?}")),
        }
    }

    let sql = match sql {
        Some(s) => s,
        None => {
            // No -c and a real terminal: the REPL. On a pipe: read stdin.
            if std::io::stdin().is_terminal() {
                // Resolve the highlight theme now, while we own the tty: the
                // "auto" appearance queries the terminal (OSC 11), which needs
                // an interactive stdin/stdout and must run before reedline does.
                theme::init(cfg.defaults.theme.as_deref(), cfg.defaults.appearance.as_deref());
                return repl::run(&conn, &target, opts);
            }
            let mut s = String::new();
            if std::io::stdin().read_to_string(&mut s).is_err() || s.trim().is_empty() {
                return fail("no SQL given: use -c \"...\" or pipe on stdin");
            }
            s
        }
    };

    if !std::io::stdout().is_terminal() && opts.mode == Mode::Duckbox {
        eprintln!("hint: boxed output on a pipe; consider --mode csv or --json");
    }
    // Piped scripts and -c may carry several statements; the protocol takes
    // one per request, so split exactly the way the REPL and .read do,
    // stopping at the first failure or interrupt.
    let mut last = Outcome::Done;
    for stmt in repl::split_statements(&sql) {
        last = run_sql(&conn, &stmt, &opts);
        if last != Outcome::Done {
            break;
        }
    }
    match last {
        Outcome::Done => ExitCode::SUCCESS,
        Outcome::Cancelled => ExitCode::from(130), // the shell convention for SIGINT
        Outcome::Failed => ExitCode::FAILURE,
    }
}

const HELP: &str = "\
pilot — the Harbor client

usage:
  pilot                        list live berths in ~/.config/harbor/runtime
  pilot <target>               interactive REPL (highlighting, Tab completion)
  pilot <target> -c \"SQL\"      run one statement
  echo \"SQL\" | pilot <target>  same, from stdin
  pilot version                print this binary's version (also -V)

target:
  name                         a config.toml entry, else a live berth: its
                               socket or registered TCP port (+ <name>.token)
  path/to.duckdb               join the owning berth, or summon one (idle-exit)
  path/to.sock                 a harbor unix socket
  http://host:port             a harbor TCP listener

options:
  --token <t>                  bearer token (else HARBOR_TOKEN, else <name>.token)
  --mode <m>                   duckbox, duckboxy, markdown, csv, json, jsonlines, line, list, trash
  --json                       shorthand for --mode jsonlines

config: $HARBOR_HOME/config.toml ([defaults] mode/timer/maxrows/nullvalue,
[connection.<name>] url|path + token-file|token-cmd). Remote TLS is Caddy's
job (PLAN.md D6); ssh is the human path to a remote berth.
";

/// Resolution order (D9/D10a): config.toml name -> live berth name ->
/// plain-HTTP url -> .duckdb path (join-or-spawn) -> socket path. Zero-config
/// local always works; the config is purely additive.
fn resolve(cfg: &config::FileConfig, target: &str, flag_token: Option<String>) -> Result<Conn, String> {
    let env_token = std::env::var("HARBOR_TOKEN").ok();

    // Explicit config entry shadows a same-named live berth (ssh_config rule).
    if let Some(entry) = cfg.connection.get(target) {
        let home = config::harbor_home();
        if berth_sock(&home, target).exists() {
            // Warn only when they actually diverge. When the entry's path IS
            // the database the live berth serves, following the config joins
            // that berth — nothing is shadowed, and warning here read as
            // "pilot is about to force a second load" to more than one user.
            let entry_db = entry.path.as_deref().map(|p| {
                let expanded = config::expand(p);
                std::fs::canonicalize(&expanded).unwrap_or(expanded)
            });
            let live_db = std::fs::read_to_string(home.join(format!("{target}.json")))
                .ok()
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                .and_then(|j| j["db"].as_str().map(PathBuf::from));
            let same = match (&entry_db, &live_db) {
                (Some(a), Some(b)) => a == b,
                _ => false,
            };
            if !same {
                eprintln!("pilot: config entry {target:?} shadows a live local berth of the same name");
            }
        }
        let token = flag_token.or(env_token).or_else(|| entry.resolve_token());
        if let Some(url) = &entry.url {
            return Ok(Conn { transport: url_transport(url)?, token });
        }
        if let Some(path) = &entry.path {
            let idle = entry.idle_exit.as_deref().unwrap_or("90s");
            let (transport, file_token) = ensure_berth(&config::expand(path), idle)?;
            return Ok(Conn { transport, token: token.or(file_token) });
        }
        return Err(format!("config entry {target:?} has neither url nor path"));
    }

    if target.starts_with("http://") || target.starts_with("https://") {
        return Ok(Conn { transport: url_transport(target)?, token: flag_token.or(env_token) });
    }
    if target.ends_with(".duckdb") {
        // D9: summon the owner. A second pilot on the same file joins the
        // same berth instead of "database is locked".
        let (transport, file_token) = ensure_berth(std::path::Path::new(target), "90s")?;
        return Ok(Conn { transport, token: flag_token.or(env_token).or(file_token) });
    }

    if target.contains('/') || target.ends_with(".sock") {
        #[cfg(unix)]
        return Ok(Conn {
            transport: Transport::Unix(PathBuf::from(target)),
            token: flag_token.or(env_token),
        });
        #[cfg(windows)]
        return Err("Unix socket targets are not supported on Windows; use a berth name or http://host:port".into());
    }

    let home = config::harbor_home();
    #[cfg(unix)]
    {
        let sock = berth_sock(&home, target);
        if sock.exists() {
            let file_token = berth_token(&home, target);
            return Ok(Conn {
                transport: Transport::Unix(sock),
                token: flag_token.or(env_token).or(file_token),
            });
        }
    }
    // A --port berth (and every Windows berth) registers its TCP address in
    // the sidecar. The bare name works for both local transports.
    if let Some(transport) = berth_tcp(&home, target) {
        let file_token = berth_token(&home, target);
        return Ok(Conn {
            transport,
            token: flag_token.or(env_token).or(file_token),
        });
    }
    Err(format!("no live berth named {target:?}{}", fleet_hint(&home)))
}

fn berth_sock(home: &std::path::Path, name: &str) -> PathBuf {
    home.join(format!("{name}.sock"))
}

fn berth_token(home: &std::path::Path, name: &str) -> Option<String> {
    let t = std::fs::read_to_string(home.join(format!("{name}.token"))).ok()?;
    let t = t.trim().to_string();
    if t.is_empty() { None } else { Some(t) }
}

/// The TCP address a --port berth registered in its sidecar json. Dials the
/// bind address, or loopback when the berth bound every interface.
fn berth_tcp(home: &std::path::Path, name: &str) -> Option<Transport> {
    let text = std::fs::read_to_string(home.join(format!("{name}.json"))).ok()?;
    let j: serde_json::Value = serde_json::from_str(&text).ok()?;
    let port = j["port"].as_u64()?;
    let bind = match j["bind"].as_str() {
        Some("0.0.0.0") | Some("::") | None => "127.0.0.1",
        Some(b) => b,
    };
    Some(Transport::Tcp(format!("{bind}:{port}")))
}

fn berth_transport(home: &std::path::Path, name: &str) -> Option<Transport> {
    #[cfg(unix)]
    {
        let sock = berth_sock(home, name);
        if sock.exists() {
            return Some(Transport::Unix(sock));
        }
    }
    berth_tcp(home, name)
}

/// Every berth the registry knows, with how to dial it — the same sidecar
/// jsons `harbor ls` reads, so socket and --port berths both appear.
fn berth_entries(home: &std::path::Path) -> Vec<(String, Transport)> {
    let mut v: Vec<(String, Transport)> = std::fs::read_dir(home)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "json"))
                .filter_map(|p| {
                    let name = p.file_stem()?.to_string_lossy().into_owned();
                    let t = berth_transport(home, &name)?;
                    Some((name, t))
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

fn url_transport(url: &str) -> Result<Transport, String> {
    if let Some(rest) = url.strip_prefix("http://") {
        let (addr, extra) = rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p));
        if !extra.is_empty() {
            return Err("path-prefixed HTTP targets are not supported; use a Harbor host:port or SSH to its socket".into());
        }
        let addr = if addr.contains(':') { addr.to_string() } else { format!("{addr}:9495") };
        return Ok(Transport::Tcp(addr));
    }
    if url.starts_with("https://") {
        return Err(
            "TLS is Caddy's job, not pilot's (PLAN.md D6): pilot stays TLS-free by design. \
             Use http:// on a trusted network, or ssh to the host and use the socket"
                .into(),
        );
    }
    Err(format!("not a url: {url}"))
}

/// Join the live berth that owns this file, or exec `harbor` to summon an
/// ephemeral one (idle-exit reaps it; see PLAN.md D9). Returns socket + the
/// berth's token, if readable.
fn ensure_berth(
    path: &std::path::Path,
    idle_exit: &str,
) -> Result<(Transport, Option<String>), String> {
    let home = config::harbor_home();
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

    // Already owned? The sidecar json says which berth claims this file.
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
                    if let Some(transport) = berth_transport(&home, name) {
                        return Ok((transport, berth_token(&home, name)));
                    }
                }
            }
        }
    }

    // Summon. Pilot never links DuckDB: the owner is the harbor binary.
    let name: String = canon
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '-' })
        .collect();
    if name.is_empty() {
        return Err(format!("cannot derive a berth name from {}", path.display()));
    }
    // The sidecar scan above found no berth owning THIS file — so a live
    // berth under the derived name is serving a DIFFERENT database. Summoning
    // would only collide on the name, and joining it would silently query the
    // wrong data. Name both files and the way out instead.
    let sidecar = home.join(format!("{name}.json"));
    if berth_sock(&home, &name).exists() || sidecar.exists() {
        let other = std::fs::read_to_string(&sidecar)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|j| j["db"].as_str().map(str::to_string))
            .unwrap_or_else(|| "another database".to_string());
        return Err(format!(
            "berth {name:?} is live but serves {other}, not {} — stop it (`harbor rm {name}`) or serve this file under a different name (`harbor add {} --name <other>`)",
            canon.display(),
            canon.display()
        ));
    }
    let harbor = std::env::var("HARBOR_BIN").unwrap_or_else(|_| "harbor".to_string());
    eprintln!("pilot: summoning a berth for {} (idle-exit {idle_exit})", canon.display());
    let status = std::process::Command::new(&harbor)
        .args(["add"])
        .arg(&canon)
        .args(["--name", &name, "--idle-exit", idle_exit])
        .status()
        .map_err(|e| format!("cannot run {harbor:?} (is harbor installed?): {e}"))?;
    if !status.success() {
        return Err(format!("harbor add failed for {}", canon.display()));
    }
    let transport = berth_transport(&home, &name)
        .ok_or_else(|| format!("harbor add returned without registering berth {name:?}"))?;
    Ok((transport, berth_token(&home, &name)))
}

fn fleet_hint(home: &std::path::Path) -> String {
    let names: Vec<String> = berth_entries(home).into_iter().map(|(n, _)| n).collect();
    if names.is_empty() { String::new() } else { format!("; live berths: {}", names.join(", ")) }
}

/// Bare `pilot`: the live local fleet view. /ready is unauthenticated by
/// design, so this needs no tokens. Named config entries resolve on demand but
/// are not merged into this list.
fn list_fleet() -> ExitCode {
    let home = config::harbor_home();
    let berths = berth_entries(&home);
    if berths.is_empty() {
        println!("no live berths in {} (start one: harbor add <db>)", home.display());
        return ExitCode::SUCCESS;
    }
    println!("{:<20} {:<8} ADDRESS", "BERTH", "STATE");
    for (name, transport) in berths {
        let state = match http::request(
            &transport,
            &endpoint::READY,
            None,
            None,
            Some(Duration::from_secs(2)),
        ) {
            Ok(r) if r.status == 200 => "ready",
            Ok(_) => "unready",
            Err(_) => "dead",
        };
        let addr = match &transport {
            #[cfg(unix)]
            Transport::Unix(p) => p.display().to_string(),
            Transport::Tcp(a) => a.clone(),
        };
        println!("{name:<20} {state:<8} {addr}");
    }
    ExitCode::SUCCESS
}

fn run_sql(conn: &Conn, sql: &str, opts: &RenderOpts) -> Outcome {
    let wall = std::time::Instant::now();
    let qid = format!("pilot-{}-{}", std::process::id(), QUERY_SEQ.fetch_add(1, Ordering::Relaxed));
    // A Ctrl-C that landed between statements (say, while the pager showed
    // the last result) aborts before this one starts — never silently clear.
    if CANCEL.swap(false, Ordering::Relaxed) {
        eprintln!("Interrupted.");
        return Outcome::Cancelled;
    }
    let body = serde_json::to_string(&SqlRequest {
        sql: sql.to_string(),
        query_id: Some(qid.clone()),
        ..Default::default()
    })
    .expect("request serializes");
    // Runs on every 250ms socket tick: paints the spinner, and turns a
    // Ctrl-C into a DELETE on the query. A second Ctrl-C while the first
    // cancel is pending means the berth is not honoring it — exit outright.
    let on_tick = {
        let fired = AtomicBool::new(false);
        let spun = AtomicU64::new(0);
        let conn = conn.clone();
        let qid = qid.clone();
        move || {
            // The spinner rides the same ticks as cancellation: half-second
            // updates on stderr, only at a terminal, erased when data lands.
            if std::io::stderr().is_terminal() {
                let halves = wall.elapsed().as_millis() as u64 / 500;
                if halves > spun.swap(halves, Ordering::Relaxed) {
                    eprint!("\r\x1b[2K… running {:.1}s (Ctrl-C cancels)", wall.elapsed().as_secs_f32());
                }
            }
            if CANCEL.swap(false, Ordering::Relaxed) {
                if !fired.swap(true, Ordering::Relaxed) {
                    clear_spinner();
                    eprintln!("Interrupted — cancelling…");
                    let _ = http::request(
                        &conn.transport,
                        &endpoint::query(&qid),
                        conn.token.as_deref(),
                        None,
                        Some(Duration::from_secs(2)),
                    );
                } else {
                    eprintln!("\npilot: second interrupt — leaving (the berth keeps cancelling)");
                    std::process::exit(130);
                }
            }
        }
    };
    let resp = match http::request_streaming(&conn.transport, &endpoint::SQL, conn.token.as_deref(), Some(&body), &on_tick) {
        Ok(r) => r,
        Err(e) => return err(&format!("cannot reach harbor: {e}")),
    };

    // Non-2xx: the body is one Event::Error document. The socket still has
    // the streaming read timeout, so ride the ticks until the body lands.
    if resp.status >= 300 {
        let status = resp.status;
        let text = read_patient(resp.body, &on_tick);
        clear_spinner();
        return match Event::parse(text.trim()) {
            Ok(Event::Error { code, .. }) if code == wire::code::CANCELLED => {
                eprintln!("Interrupted.");
                Outcome::Cancelled
            }
            Ok(Event::Error { code, message }) => err(&format!("harbor error ({code}): {message}")),
            _ => err(&format!("HTTP {status} from harbor: {text}")),
        };
    }

    // Stream the envelope through the renderer: pipe modes emit per row,
    // boxed modes retain O(display) and draw after `end` (render.rs).
    clear_spinner();
    let mut renderer = Renderer::new(opts);
    let mut body = resp.body;
    let mut acc: Vec<u8> = Vec::new();
    // How far into `acc` the newline search has already looked. Without it the
    // scan restarted at byte 0 on every 8 KiB read, so one wide row — a large
    // VARCHAR, a big STRUCT — cost O(row²) byte comparisons to find its single
    // terminator. Bytes already examined cannot grow a newline later.
    let mut scanned = 0usize;
    let mut chunk = [0u8; 8192];
    loop {
        // Reading bytes, not read_line: the socket ticks every 250ms
        // (request_streaming) and a tick can land mid-line. read_line decodes
        // UTF-8 as it goes, so a tick arriving in the middle of a multi-byte
        // char (CJK/emoji) makes its guard consume those bytes, fail, and
        // return InvalidData — aborting a perfectly healthy stream and dropping
        // data. A byte accumulator has no char-boundary dependency: partial
        // bytes simply wait in `acc` for the next read.
        let found = acc[scanned..].iter().position(|&b| b == b'\n').map(|p| scanned + p);
        let Some(nl) = found else {
            scanned = acc.len();
            match body.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => acc.extend_from_slice(&chunk[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    on_tick();
                }
                Err(e) => return err(&format!("stream died: {e}")),
            }
            continue;
        };
        let text = String::from_utf8_lossy(&acc[..=nl]).into_owned();
        acc.drain(..=nl);
        scanned = 0;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        let event = match Event::parse(trimmed) {
            Ok(ev) => ev,
            Err(e) => return err(&format!("bad envelope line ({e}): {trimmed}")),
        };
        match event {
            Event::Schema { columns } => renderer.schema(&columns),
            Event::Row { values } => {
                renderer.row(values);
                if let Some(kind) = renderer.failed() {
                    // Stop reading; dropping the connection tells the berth.
                    // A closed pipe (`pilot … | head`) is the Unix goodbye,
                    // not an error; anything else gets reported.
                    return if kind == std::io::ErrorKind::BrokenPipe {
                        Outcome::Done
                    } else {
                        err(&format!("writing output failed: {kind}"))
                    };
                }
            }
            Event::End { row_count, time_ms } => {
                return match renderer.end(row_count, time_ms, wall.elapsed().as_millis()) {
                    Ok(()) => Outcome::Done,
                    Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Outcome::Done,
                    Err(e) => err(&format!("writing output failed: {e}")),
                };
            }
            Event::Error { code, .. } if code == wire::code::CANCELLED => {
                clear_spinner();
                eprintln!("Interrupted.");
                return Outcome::Cancelled;
            }
            Event::Error { code, message } => {
                return err(&format!("harbor error ({code}): {message}"));
            }
        }
    }
    err("stream ended without an end event")
}

/// Read a whole (small) body over a socket that has the streaming tick
/// timeout, retrying through the ticks with a hard 5s ceiling.
fn read_patient(mut body: Box<dyn BufRead>, on_tick: &dyn Fn()) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match body.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                on_tick();
                if std::time::Instant::now() > deadline {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Erase a spinner remnant before printing anything else on stderr. The
/// spinner paints `\r\x1b[2K… running …` on every half-second tick and only at
/// a terminal, so a line left mid-spin would otherwise have the next message
/// (an error, or `Interrupted.`) glued to its tail. No-op when stderr is not a
/// terminal — nothing was painted.
fn clear_spinner() {
    if std::io::stderr().is_terminal() {
        eprint!("\r\x1b[2K");
    }
}

fn err(msg: &str) -> Outcome {
    clear_spinner();
    eprintln!("pilot: {msg}");
    Outcome::Failed
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("pilot: {msg}");
    ExitCode::FAILURE
}
