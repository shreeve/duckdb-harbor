//! The client half of the one binary — the REPL, the one-shot runner, and
//! the list.
//!
//! Zero config, zero registry. A target is a database file, a socket, or a
//! plain-HTTP url; nothing else. A file with no server behind it gets one
//! spawned on the spot — detached, refcounted, gone when its last client
//! leaves — so `harbor data.duckdb` always just opens the database, exactly
//! like the duckdb shell, except everyone else can be in it too.
//!
//!   harbor <db.duckdb>              the REPL (or stdin/-c on a pipe)
//!   harbor <path/to.sock>           a harbor unix socket
//!   harbor http://host:port         a harbor TCP listener
//!
//! TLS is Caddy's job — the client speaks plain HTTP over UDS/TCP.

mod complete;
mod render;
mod highlight;
mod http;
mod interactive;
mod keywords;
mod scan;
mod theme;

pub use http::Transport;

use wire::{Event, SqlRequest, endpoint};
use render::{Mode, RenderOpts, Renderer};
use std::io::{BufRead, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Set by SIGINT while a statement streams (registered in cli_main/helm);
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

/// The client's whole CLI: everything except bare `harbor` (the list, which
/// main dispatches straight to list_main) and `<db> start` (the server).
pub fn cli_main(args: impl IntoIterator<Item = String>) -> ExitCode {
    // Ctrl-C cancels the running statement (via its queryId), it does not
    // kill the client. At the REPL prompt reedline runs raw mode, so SIGINT
    // only fires while a statement streams; a second Ctrl-C while the first
    // cancel is still pending exits outright (the tick handler enforces it).
    let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, CANCEL.clone());
    let mut args = args.into_iter();
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
            _ if target.is_none() && !a.starts_with('-') => target = Some(a),
            _ => return fail(&format!("unexpected argument: {a}")),
        }
    }

    let Some(target) = target else {
        return fail("which database? (harbor <db.duckdb> — or bare harbor to see what's running)");
    };
    let conn = match resolve(&target, token) {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    // The mooring, held for the life of this invocation: a spawned server
    // lives while anyone is connected, and between statements — a human
    // thinking at the prompt, a script paused mid-pipe — this silent open
    // connection is the "anyone".
    let anchor = http::hold(&conn.transport);

    let mut opts = RenderOpts::default();
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
                theme::init(None, None);
                return interactive::run(&conn, &prompt_name(&target), opts, anchor);
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
    for stmt in interactive::split_statements(&sql) {
        last = run_sql(&conn, &stmt, &opts);
        if last != Outcome::Done {
            break;
        }
    }
    drop(anchor);
    match last {
        Outcome::Done => ExitCode::SUCCESS,
        Outcome::Cancelled => ExitCode::from(130), // the shell convention for SIGINT
        Outcome::Failed => ExitCode::FAILURE,
    }
}

const HELP: &str = "\
harbor — a DuckDB database, served

usage:
  harbor                       what's running
  harbor <db.duckdb>           open a database: the REPL on a terminal, or
                               SQL from -c \"...\" / stdin on a pipe. No server
                               behind the file yet? One is spawned for it —
                               it lives while anyone is connected.
  harbor <path/to.sock>        a harbor unix socket
  harbor http://host:port      a harbor TCP listener
  harbor <db.duckdb> start     start it yourself — the server is yours, and
                               lives until you leave (harbor <db> start -h)

options:
  -c \"SQL\"                     run statements and exit (stdin works too)
  --token <t>                  bearer token for a TCP server (else $HARBOR_TOKEN)
  --mode <m>                   duckbox, duckboxy, markdown, csv, json, jsonlines, line, list, trash
  --json                       shorthand for --mode jsonlines

Remote TLS is Caddy's job; ssh is the human path to a remote host.
";

/// A target is spelled out or it is refused: a plain-HTTP url, a socket, or
/// a database file (a path carries a slash or a dot; there are no names).
fn resolve(target: &str, flag_token: Option<String>) -> Result<Conn, String> {
    let token = flag_token.or_else(|| std::env::var("HARBOR_TOKEN").ok());

    if target.starts_with("http://") || target.starts_with("https://") {
        return Ok(Conn { transport: url_transport(target)?, token });
    }
    if !harbor_common::looks_like_path(target) {
        return Err(format!(
            "{target:?} is not a database file (a path carries a / or a dot) — \
             harbor takes a file, a .sock, or http://host:port"
        ));
    }
    let p = harbor_common::paths::expand(target);
    // The filesystem says which kind of path this is: a live socket is
    // dialled, and everything else is a database file to open. `.sock`
    // still reads as a socket when the file is not there yet, so a
    // mistyped socket path fails as a socket, loudly, instead of
    // quietly becoming a fresh database.
    if is_socket(&p) || target.ends_with(".sock") {
        #[cfg(unix)]
        return Ok(Conn { transport: Transport::Unix(p), token });
        #[cfg(windows)]
        return Err("Unix socket targets are not supported on Windows; use http://host:port".into());
    }
    Ok(Conn { transport: ensure_server(&p)?, token })
}

/// Join the server that owns this file, or spawn one — this same binary,
/// detached and refcounted: it lives while anyone is connected and takes its
/// socket with it when the last client leaves. The socket is identity, not
/// registry: derived from the file's canonical path, so every spelling of the
/// same file lands on the same server and no scan or sidecar is needed.
fn ensure_server(path: &Path) -> Result<Transport, String> {
    #[cfg(windows)]
    {
        let _ = path;
        return Err(
            "spawn-on-use needs unix sockets; on Windows run `harbor <db> start --port <p> --token <t>` \
             and connect to http://127.0.0.1:<p>"
                .into(),
        );
    }
    #[cfg(unix)]
    {
        let runtime = harbor_common::runtime_dir()?;
        let canon = harbor_common::paths::canonical_db(path)?;
        let sock = harbor_common::socket_for(&runtime, &canon)?;
        let transport = Transport::Unix(sock.clone());
        if ready(&transport) {
            return Ok(transport);
        }

        // Spawn. Same binary, no PATH lookup, no environment contract —
        // current_exe is the whole story. Detached (own process group, no
        // tty), stderr to a log beside the socket so a failure has a face.
        harbor_common::perms::ensure_private_dir(&runtime)?;
        let log_path = sock.with_extension("log");
        let log = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&log_path)
            .map_err(|e| format!("log file: {e}"))?;
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        let mut cmd = std::process::Command::new(exe);
        cmd.arg(&canon).arg("start");
        // The private lifetime signal: a summoned server is refcounted, so it
        // leaves when idle. It rides an env channel, not the command line —
        // ephemerality is something membership says (a detached start), never a
        // flag, and a spawn is not a verb the user typed.
        cmd.env("HARBOR_EPHEMERAL", "1");
        // A typed path is the duckdb-cli contract: start opens it existing or
        // not, so a summoned database that isn't there yet is simply created.
        {
            use std::os::unix::process::CommandExt;
            use std::process::Stdio;
            cmd.stdin(Stdio::null())
                .stdout(Stdio::from(log.try_clone().map_err(|e| e.to_string())?))
                .stderr(Stdio::from(log))
                .process_group(0); // detached from our tty/session
        }
        let mut child = cmd.spawn().map_err(|e| format!("spawn: {e}"))?;

        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if ready(&transport) {
                return Ok(transport);
            }
            // The child dying is an answer, not a timeout — but it can be the
            // GOOD answer: two clients raced, ours lost the database lock to
            // the winner, and the winner's socket (same derived path) serves
            // us fine. Only a dead child AND no listener is a failure.
            if let Ok(Some(status)) = child.try_wait() {
                if ready(&transport) {
                    return Ok(transport);
                }
                return Err(format!(
                    "the server did not start ({status}) — {}",
                    log_tail(&log_path)
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(format!("{} did not come up in 15s — {}", canon.display(), log_tail(&log_path)))
    }
}

/// The last few log lines, inlined — the operator should not have to go
/// find the file to learn why their prompt never appeared.
fn log_tail(log_path: &Path) -> String {
    match std::fs::read_to_string(log_path) {
        Ok(s) if !s.trim().is_empty() => {
            let tail: Vec<&str> = s.lines().rev().take(3).collect();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            format!("its log says:\n        {}", tail.join("\n        "))
        }
        _ => format!("see {}", log_path.display()),
    }
}

/// Is a harbor answering on this socket right now? start's pre-flight, for
/// a friendlier refusal than the database lock error.
#[cfg(unix)]
pub fn sock_ready(sock: &Path) -> bool {
    ready(&Transport::Unix(sock.to_path_buf()))
}

/// Stop the server for a database FILE, if one is running. Never spawns —
/// stopping a stopped berth is a quiet no-op. Returns whether a server was
/// actually there to stop. The `stop` verb's whole implementation.
#[cfg(unix)]
pub fn shutdown(db: &Path) -> Result<bool, String> {
    if !db.exists() {
        return Ok(false); // no file, nothing behind it
    }
    let runtime = harbor_common::runtime_dir()?;
    let canon = harbor_common::paths::canonical_db(db)?;
    let transport = Transport::Unix(harbor_common::socket_for(&runtime, &canon)?);
    if !ready(&transport) {
        return Ok(false); // nothing answering on its socket
    }
    // POST /shutdown drains, checkpoints, and exits. The server can close the
    // socket as it goes, so a dropped connection right after the request is
    // success, not failure — re-probe to be sure which it was.
    match http::request(&transport, &endpoint::SHUTDOWN, None, None, Some(Duration::from_secs(30))) {
        Ok(_) => Ok(true),
        Err(_) if !ready(&transport) => Ok(true),
        Err(e) => Err(format!("stop: {e}")),
    }
}

#[cfg(windows)]
pub fn shutdown(_db: &Path) -> Result<bool, String> {
    Err("a TCP server is stopped by its own SIGTERM, not over a socket".into())
}

/// GET /ready, 200 or bust. Unauthenticated by design, so no token needed.
fn ready(transport: &Transport) -> bool {
    matches!(
        http::request(transport, &endpoint::READY, None, None, Some(Duration::from_secs(2))),
        Ok(r) if r.status == 200
    )
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
            "TLS is Caddy's job, not harbor's: the client stays TLS-free by design. \
             Use http:// on a trusted network, or ssh to the host and use the socket"
                .into(),
        );
    }
    Err(format!("not a url: {url}"))
}

/// The short name a prompt wears: a path or socket shows its stem, a url
/// drops its scheme. The prompt orients; the full target is in the list.
pub fn prompt_name(target: &str) -> String {
    if let Some(rest) = target.split_once("://").map(|(_, r)| r) {
        return rest.trim_end_matches('/').to_string();
    }
    if harbor_common::looks_like_path(target)
        && let Some(stem) = Path::new(target).file_stem()
    {
        return stem.to_string_lossy().into_owned();
    }
    target.to_string()
}

/// Does this path exist as a unix socket right now?
fn is_socket(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        std::fs::metadata(p).map(|m| m.file_type().is_socket()).unwrap_or(false)
    }
    #[cfg(windows)]
    {
        let _ = p;
        false
    }
}

// ---------------------------------------------------------------------------
// The list — bare `harbor`
// ---------------------------------------------------------------------------

/// One live server, as its own /info tells it.
struct ListRow {
    database: String,
    pid: String,
    clients: String,
    uptime: String,
    address: String,
}

/// Bare `harbor`: what's running, straight from the filesystem and the
/// servers themselves. Readdir the runtime dir for sockets, ask each for
/// /info, and unlink the ones nothing answers on — the registry IS the
/// listening socket, so a stale file is litter, not state.
pub fn list_main() -> ExitCode {
    match list() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e),
    }
}

fn list() -> Result<(), String> {
    let runtime = harbor_common::runtime_dir()?;
    let mut socks: Vec<PathBuf> = match std::fs::read_dir(&runtime) {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "sock"))
            .collect(),
        Err(_) => Vec::new(), // no runtime dir yet: nothing has ever served
    };
    socks.sort();

    let mut rows: Vec<ListRow> = Vec::new();
    for sock in socks {
        let transport = {
            #[cfg(unix)]
            {
                Transport::Unix(sock.clone())
            }
            #[cfg(windows)]
            {
                continue;
            }
        };
        let shown = harbor_common::paths::shorten(&sock);
        match http::request(&transport, &endpoint::INFO, None, None, Some(Duration::from_secs(2))) {
            Ok(r) if r.status == 200 => {
                let v: serde_json::Value =
                    serde_json::from_str(r.body_string().unwrap_or_default().trim())
                        .unwrap_or_default();
                let ms = v["uptimeMs"].as_u64().unwrap_or(0);
                rows.push(ListRow {
                    database: v["database"]
                        .as_str()
                        .map(|d| harbor_common::paths::shorten(Path::new(d)))
                        .unwrap_or_else(|| "?".into()),
                    pid: v["pid"].as_u64().map_or_else(|| "?".into(), |p| p.to_string()),
                    clients: v["clients"].as_u64().map_or_else(|| "?".into(), |c| c.to_string()),
                    uptime: harbor_common::duration::humanize(Duration::from_millis(ms)),
                    address: shown,
                });
            }
            // It answered, just not with an open /info (an older, token'd
            // server, say). Alive is alive — show the row, claim nothing.
            Ok(_) => rows.push(ListRow {
                database: "?".into(),
                pid: "?".into(),
                clients: "?".into(),
                uptime: "?".into(),
                address: shown,
            }),
            // Refused means nothing listens: a leftover from a kill -9 or a
            // crash. Anything else (a transient error, a permission oddity)
            // proves nothing, and an unlink on "proves nothing" is how a live
            // server loses its front door.
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                let _ = std::fs::remove_file(&sock);
            }
            Err(_) => {}
        }
    }

    if rows.is_empty() {
        println!("Nothing running.\n");
        println!("  harbor <db.duckdb>   open a database — served while anyone is connected");
        return Ok(());
    }

    // The fleet as one box: the database name itself carries status — green
    // when its server answered /info (running and readable), dim when it
    // answered without one (an older, token-gated server — alive, but mute)
    // or isn't running. No status dot here: the terminal tints the NAME (a
    // dot sharing that tone would be pure redundancy), whereas DuckTable's
    // sidebar uses a plain name + a colored dot — same meaning, form suited
    // to each surface. PID/CLIENTS/UPTIME right-align under their heads, and
    // the long socket path hangs below the grid as a footnote, so the columns
    // stay tight and the address is still one glance away. A tally closes it,
    // live rows first. Piped output degrades to plain text (Style::stdout).
    use harbor_common::ui::{Cell, Style, Table, Tone};
    let mut t = Table::new(["DATABASE", "PID", "CLIENTS", "UPTIME"]);
    for r in &rows {
        let running = r.pid != "?";
        t.row([
            Cell::new(&r.database).tone(if running { Tone::Green } else { Tone::Dim }),
            Cell::new(&r.pid).right(),
            Cell::new(&r.clients).right(),
            Cell::new(&r.uptime).right(),
        ]);
        t.note(Tone::Dim, &r.address);
    }
    println!("{}", t.render(&Style::stdout()));

    let running = rows.iter().filter(|r| r.pid != "?").count();
    let mute = rows.len() - running;
    let mut parts = Vec::new();
    if running > 0 {
        parts.push(format!("{running} running"));
    }
    if mute > 0 {
        parts.push(format!("{mute} unreadable"));
    }
    println!("  {}\n", parts.join(", "));
    Ok(())
}

// ---------------------------------------------------------------------------
// The helm — `harbor <db> start` on a terminal
// ---------------------------------------------------------------------------

/// The prompt a foreground server wears: the same REPL, dialled at the
/// server's own front door, so what the operator types is what any client
/// would get. Returning ends the server — the caller stops and waits — which
/// is the foreground-start doctrine: the server is yours, it lives until you
/// leave.
pub fn helm(transport: Transport, token: Option<String>, name: &str) {
    let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, CANCEL.clone());
    theme::init(None, None);
    let conn = Conn { transport, token };
    // No anchor: an owned server's lifetime is the operator's presence at
    // this prompt, not its client count.
    let _ = interactive::run(&conn, name, RenderOpts::default(), None);
}

fn run_sql(conn: &Conn, sql: &str, opts: &RenderOpts) -> Outcome {
    let wall = std::time::Instant::now();
    let qid = format!("cli-{}-{}", std::process::id(), QUERY_SEQ.fetch_add(1, Ordering::Relaxed));
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
    // cancel is pending means the server is not honoring it — exit outright.
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
                    eprintln!("\nharbor: second interrupt — leaving (the server keeps cancelling)");
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
                    // Stop reading; dropping the connection tells the server.
                    // A closed pipe (`harbor … | head`) is the Unix goodbye,
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
    eprintln!("harbor: {msg}");
    Outcome::Failed
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("harbor: {msg}");
    ExitCode::FAILURE
}
