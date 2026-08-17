//! pilot — the Harbor client.
//!
//! Zero-config local (D10a): a bare name resolves to ~/.harbor/<name>.sock
//! and its token file; config.toml is purely additive (remotes, aliases,
//! taste). TLS is Caddy's job (D6) — pilot speaks plain HTTP over UDS/TCP.
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

use harbor_protocol::{Event, SqlRequest, endpoint};
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
            _ if target.is_none() => target = Some(a),
            _ => return fail(&format!("unexpected argument: {a}")),
        }
    }

    let Some(target) = target else { return list_fleet() };

    let conn = match resolve(&target, token) {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };

    let sql = match sql {
        Some(s) => s,
        None => {
            // No -c and a real terminal: the REPL. On a pipe: read stdin.
            if std::io::stdin().is_terminal() {
                return repl::run(&conn, &target);
            }
            let mut s = String::new();
            if std::io::stdin().read_to_string(&mut s).is_err() || s.trim().is_empty() {
                return fail("no SQL given: use -c \"...\" or pipe on stdin");
            }
            s
        }
    };

    let mut opts = RenderOpts::with_defaults(&config::load().defaults);
    if json {
        opts.mode = Mode::JsonLines;
    }
    if let Some(m) = mode {
        match Mode::parse(&m) {
            Some(m) => opts.mode = m,
            None => return fail(&format!("unknown mode {m:?}")),
        }
    }
    if !std::io::stdout().is_terminal() && opts.mode == Mode::Duckbox {
        eprintln!("hint: boxed output on a pipe; consider --mode csv or --json");
    }
    match run_sql(&conn, sql.trim(), &opts) {
        Outcome::Done => ExitCode::SUCCESS,
        Outcome::Cancelled => ExitCode::from(130), // the shell convention for SIGINT
        Outcome::Failed => ExitCode::FAILURE,
    }
}

const HELP: &str = "\
pilot — the Harbor client

usage:
  pilot                        list live berths in ~/.harbor
  pilot <target>               interactive REPL (highlighting, Tab completion)
  pilot <target> -c \"SQL\"      run one statement
  echo \"SQL\" | pilot <target>  same, from stdin

target:
  name                         a config.toml entry, else a live berth:
                               ~/.harbor/<name>.sock (+ <name>.token)
  path/to.duckdb               join the owning berth, or summon one (idle-exit)
  path/to.sock                 a harbor unix socket
  http://host:port             a harbor TCP listener

options:
  --token <t>                  bearer token (else HARBOR_TOKEN, else <name>.token)
  --mode <m>                   duckbox, markdown, csv, json, jsonlines, line, list, trash
  --json                       shorthand for --mode jsonlines

config: $HARBOR_HOME/config.toml ([defaults] mode/timer/maxrows/nullvalue,
[connection.<name>] url|path + token-file|token-cmd). Remote TLS is Caddy's
job (PLAN.md D6); ssh is the human path to a remote berth.
";

/// Resolution order (D9/D10a): config.toml name -> live berth name ->
/// http(s) url -> .duckdb path (join-or-spawn) -> socket path. Zero-config
/// local always works; the config is purely additive.
fn resolve(target: &str, flag_token: Option<String>) -> Result<Conn, String> {
    let env_token = std::env::var("HARBOR_TOKEN").ok();
    let cfg = config::load();

    // Explicit config entry shadows a same-named live berth (ssh_config rule).
    if let Some(entry) = cfg.connection.get(target) {
        let home = http::harbor_home();
        if home.join(format!("{target}.sock")).exists() {
            eprintln!("pilot: config entry {target:?} shadows a live local berth of the same name");
        }
        let token = flag_token.or(env_token).or_else(|| entry.resolve_token());
        if let Some(url) = &entry.url {
            return Ok(Conn { transport: url_transport(url)?, token });
        }
        if let Some(path) = &entry.path {
            let idle = entry.idle_exit.as_deref().unwrap_or("90s");
            let (sock, file_token) = ensure_berth(&config::expand(path), idle)?;
            return Ok(Conn { transport: Transport::Unix(sock), token: token.or(file_token) });
        }
        return Err(format!("config entry {target:?} has neither url nor path"));
    }

    if target.starts_with("http://") || target.starts_with("https://") {
        return Ok(Conn { transport: url_transport(target)?, token: flag_token.or(env_token) });
    }
    if target.ends_with(".duckdb") {
        // D9: summon the owner. A second pilot on the same file joins the
        // same berth instead of "database is locked".
        let (sock, file_token) = ensure_berth(std::path::Path::new(target), "90s")?;
        return Ok(Conn { transport: Transport::Unix(sock), token: flag_token.or(env_token).or(file_token) });
    }

    let sock: PathBuf;
    let mut file_token = None;
    if target.contains('/') || target.ends_with(".sock") {
        sock = PathBuf::from(target);
    } else {
        let home = http::harbor_home();
        sock = home.join(format!("{target}.sock"));
        if !sock.exists() {
            return Err(format!(
                "no live berth named {target:?} ({} not found){}",
                sock.display(),
                fleet_hint(&home)
            ));
        }
        file_token = std::fs::read_to_string(home.join(format!("{target}.token")))
            .ok()
            .map(|t| t.trim().to_string());
    }
    Ok(Conn { transport: Transport::Unix(sock), token: flag_token.or(env_token).or(file_token) })
}

fn url_transport(url: &str) -> Result<Transport, String> {
    if let Some(rest) = url.strip_prefix("http://") {
        let (addr, extra) = rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p));
        if !extra.is_empty() {
            return Err("path-prefixed http targets are not supported; use https via Caddy or host:port".into());
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
fn ensure_berth(path: &std::path::Path, idle_exit: &str) -> Result<(PathBuf, Option<String>), String> {
    let home = http::harbor_home();
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
                    let sock = home.join(format!("{name}.sock"));
                    if sock.exists() {
                        let tok = std::fs::read_to_string(home.join(format!("{name}.token")))
                            .ok()
                            .map(|t| t.trim().to_string());
                        return Ok((sock, tok));
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
    let sock = home.join(format!("{name}.sock"));
    let tok = std::fs::read_to_string(home.join(format!("{name}.token")))
        .ok()
        .map(|t| t.trim().to_string());
    Ok((sock, tok))
}

fn fleet_hint(home: &std::path::Path) -> String {
    let names: Vec<String> = berth_sockets(home)
        .iter()
        .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .collect();
    if names.is_empty() { String::new() } else { format!("; live berths: {}", names.join(", ")) }
}

fn berth_sockets(home: &std::path::Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(home)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "sock"))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Bare `pilot`: the fleet view. /ready is unauthenticated by design, so this
/// needs no tokens; config-file remotes join this view in Phase 2.
fn list_fleet() -> ExitCode {
    let home = http::harbor_home();
    let socks = berth_sockets(&home);
    if socks.is_empty() {
        println!("no live berths in {} (start one: harbor add <db>)", home.display());
        return ExitCode::SUCCESS;
    }
    println!("{:<20} {:<8} SOCKET", "BERTH", "STATE");
    for sock in socks {
        let name = sock.file_stem().unwrap_or_default().to_string_lossy();
        let state = match http::request(
            &Transport::Unix(sock.clone()),
            "GET",
            endpoint::READY,
            None,
            None,
            Some(Duration::from_secs(2)),
        ) {
            Ok(r) if r.status == 200 => "ready",
            Ok(_) => "unready",
            Err(_) => "dead",
        };
        println!("{name:<20} {state:<8} {}", sock.display());
    }
    ExitCode::SUCCESS
}

fn run_sql(conn: &Conn, sql: &str, opts: &RenderOpts) -> Outcome {
    let wall = std::time::Instant::now();
    let qid = format!("pilot-{}-{}", std::process::id(), QUERY_SEQ.fetch_add(1, Ordering::Relaxed));
    CANCEL.store(false, Ordering::Relaxed);
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
                    eprint!("\r\x1b[2K");
                    eprintln!("Interrupted — cancelling…");
                    let _ = http::request(
                        &conn.transport,
                        "DELETE",
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
    let resp = match http::request_streaming(&conn.transport, "POST", endpoint::SQL, conn.token.as_deref(), Some(&body), &on_tick) {
        Ok(r) => r,
        Err(e) => return err(&format!("cannot reach harbor: {e}")),
    };

    // Non-2xx: the body is one Event::Error document. The socket still has
    // the streaming read timeout, so ride the ticks until the body lands.
    if resp.status >= 300 {
        let status = resp.status;
        let text = read_patient(resp.body, &on_tick);
        return match Event::parse(text.trim()) {
            Ok(Event::Error { code, .. }) if code == harbor_protocol::code::CANCELLED => {
                eprintln!("Interrupted.");
                Outcome::Cancelled
            }
            Ok(Event::Error { code, message }) => err(&format!("harbor error ({code}): {message}")),
            _ => err(&format!("HTTP {status} from harbor: {text}")),
        };
    }

    // Stream the envelope through the renderer: pipe modes emit per row,
    // boxed modes retain O(display) and draw after `end` (render.rs).
    if std::io::stderr().is_terminal() {
        eprint!("\r\x1b[2K"); // clear any spinner remnant
    }
    let mut renderer = Renderer::new(opts);
    let mut body = resp.body;
    let mut line = String::new();
    loop {
        // The socket ticks every 250ms (request_streaming); a tick is where a
        // Ctrl-C gets noticed. Partial reads stay in `line` across ticks.
        match body.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                on_tick();
                continue;
            }
            Err(e) => return err(&format!("stream died: {e}")),
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            line.clear();
            continue;
        }
        let event = match Event::parse(trimmed) {
            Ok(ev) => ev,
            Err(e) => return err(&format!("bad envelope line ({e}): {trimmed}")),
        };
        line.clear();
        match event {
            Event::Schema { columns } => renderer.schema(&columns),
            Event::Row { values } => renderer.row(values),
            Event::End { row_count, time_ms } => {
                renderer.end(row_count, time_ms, wall.elapsed().as_millis());
                return Outcome::Done;
            }
            Event::Error { code, .. } if code == harbor_protocol::code::CANCELLED => {
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

fn err(msg: &str) -> Outcome {
    eprintln!("pilot: {msg}");
    Outcome::Failed
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("pilot: {msg}");
    ExitCode::FAILURE
}
