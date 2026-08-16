//! harbor — DuckDB wearing a server.
//!
//! One binary, two jobs (PLAN.md D8): `serve` embeds DuckDB and owns one
//! database file; the fleet verbs (`add`, `ls`, `stop`, `rm`) manage the
//! berths of ~/.harbor from outside, linking no engine code paths at all.
//!
//!   harbor serve db.duckdb [--name n] [--socket p | --port p] [--token t]
//!   harbor add   db.duckdb [--name n]        spawn a detached berth, wait ready
//!   harbor ls                                the live fleet
//!   harbor stop  <name>                      SIGTERM → drain, CHECKPOINT
//!   harbor rm    <name>                      stop + clear registry (never the db)
//!
//! The registry is the filesystem (D3): <name>.sock is the registration,
//! <name>.lock (flock) is the mutex, <name>.json is identity, <name>.token
//! is the credential. No daemon anywhere.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match args.split_first() {
        Some((c, r)) => (c.as_str(), r.to_vec()),
        None => {
            print!("{HELP}");
            return ExitCode::SUCCESS;
        }
    };
    let result = match cmd {
        "serve" => serve(rest),
        "add" => add(rest),
        "ls" => ls(),
        "stop" => stop_berth(rest, false),
        "rm" => stop_berth(rest, true),
        "-h" | "--help" | "help" => {
            print!("{HELP}");
            Ok(())
        }
        other => Err(format!("unknown command {other:?} (try: harbor --help)")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("harbor: {e}");
            ExitCode::FAILURE
        }
    }
}

const HELP: &str = "\
harbor — DuckDB wearing a server

usage:
  harbor serve <db.duckdb> [opts]   own a database, serve it (foreground)
  harbor add   <db.duckdb> [opts]   spawn a detached berth, wait until ready
  harbor ls                         list berths in the harbor
  harbor stop  <name>               SIGTERM the berth: drain, CHECKPOINT, exit
  harbor rm    <name>               stop + remove registry entries (never the db)

serve/add options:
  --name <n>          berth name (default: db file stem)
  --socket <path>     unix socket (default: $HARBOR_HOME/<name>.sock)
  --port <p>          listen on TCP 127.0.0.1:<p> instead of a unix socket
  --bind <addr>       TCP bind address (with --port; default 127.0.0.1)
  --token <t>         bearer token ('' disables auth; default: <name>.token,
                      minted on first serve)
  --workers <n>       executor pool size (default 6)
  --memory-limit <s>  DuckDB memory_limit (default 2GB — fleet-safe, D2)
  --threads <n>       DuckDB threads (default: DuckDB's own)
  --attach <n>=<p>    ATTACH another database file as catalog <n> (repeatable;
                      append :ro for read-only) — the cross-db-join case (D2)
  --idle-exit <d>     drain, CHECKPOINT and exit after <d> (e.g. 90s, 10m) with
                      no requests and no live sessions (D9 ephemeral berths)
  --log               log requests to stderr
";

struct Opts {
    db: PathBuf,
    name: String,
    socket: Option<PathBuf>,
    port: Option<u16>,
    bind: String,
    token: Option<String>, // None = use/mint <name>.token; Some("") = auth off
    workers: usize,
    memory_limit: String,
    threads: Option<u32>,
    attach: Vec<String>,
    idle_exit: Option<Duration>,
    log: bool,
}

/// "90s", "10m", "2h", or bare seconds.
fn parse_duration(s: &str) -> Result<Duration, String> {
    let (num, unit) = s.split_at(s.len() - s.chars().last().map_or(0, |c| c.is_alphabetic() as usize));
    let n: u64 = num.parse().map_err(|_| format!("bad duration {s:?}"))?;
    let secs = match unit {
        "" | "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        _ => return Err(format!("bad duration unit in {s:?} (use s, m, h)")),
    };
    Ok(Duration::from_secs(secs))
}

fn parse_opts(rest: Vec<String>) -> Result<Opts, String> {
    let mut it = rest.into_iter();
    let mut db: Option<PathBuf> = None;
    let mut o = Opts {
        db: PathBuf::new(),
        name: String::new(),
        socket: None,
        port: None,
        bind: "127.0.0.1".into(),
        token: None,
        workers: harbor_core::DEFAULT_MAX_INFLIGHT,
        memory_limit: "2GB".into(),
        threads: None,
        attach: Vec::new(),
        idle_exit: None,
        log: false,
    };
    let mut named: Option<String> = None;
    while let Some(a) = it.next() {
        let mut take = |what: &str| it.next().ok_or(format!("--{what} needs a value"));
        match a.as_str() {
            "--name" => named = Some(take("name")?),
            "--socket" => o.socket = Some(PathBuf::from(take("socket")?)),
            "--port" => o.port = Some(take("port")?.parse().map_err(|_| "bad --port")?),
            "--bind" => o.bind = take("bind")?,
            "--token" => o.token = Some(take("token")?),
            "--workers" => o.workers = take("workers")?.parse().map_err(|_| "bad --workers")?,
            "--memory-limit" => o.memory_limit = take("memory-limit")?,
            "--threads" => o.threads = Some(take("threads")?.parse().map_err(|_| "bad --threads")?),
            "--attach" => o.attach.push(take("attach")?),
            "--idle-exit" => o.idle_exit = Some(parse_duration(&take("idle-exit")?)?),
            "--log" => o.log = true,
            _ if db.is_none() && !a.starts_with('-') => db = Some(PathBuf::from(a)),
            other => return Err(format!("unexpected argument: {other}")),
        }
    }
    o.db = db.ok_or("no database file given")?;
    o.name = match named {
        Some(n) => n,
        None => o
            .db
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    o.name = normalize(&o.name)?;
    Ok(o)
}

/// Berth names are registry filenames: [a-z0-9_-], 1..=64.
fn normalize(name: &str) -> Result<String, String> {
    let n: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '-' })
        .collect();
    if n.is_empty() || n.len() > 64 {
        return Err(format!("bad berth name {name:?}"));
    }
    Ok(n)
}

fn harbor_home() -> Result<PathBuf, String> {
    let home = match std::env::var("HARBOR_HOME") {
        Ok(h) => PathBuf::from(h),
        Err(_) => {
            let h = std::env::var("HOME").map_err(|_| "no $HOME")?;
            Path::new(&h).join(".harbor")
        }
    };
    if !home.exists() {
        std::fs::create_dir_all(&home).map_err(|e| format!("cannot create {}: {e}", home.display()))?;
        let _ = chmod(&home, 0o700);
    }
    Ok(home)
}

fn chmod(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

// ---------------------------------------------------------------------------
// serve — the only verb that touches the engine
// ---------------------------------------------------------------------------

fn serve(rest: Vec<String>) -> Result<(), String> {
    let o = parse_opts(rest)?;
    let home = harbor_home()?;

    // The flock on <name>.lock is the real mutex (D3): held for process life;
    // if we hold it and the socket file exists, the socket is stale by
    // definition and safe to unlink. Never unlink without the lock.
    let lock_path = home.join(format!("{}.lock", o.name));
    let lock = std::fs::File::create(&lock_path).map_err(|e| format!("lock: {e}"))?;
    unsafe {
        use std::os::fd::AsRawFd;
        if libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) != 0 {
            return Err(format!("berth {:?} is already claimed in {}", o.name, home.display()));
        }
    }
    std::mem::forget(lock); // hold the flock until the process exits

    let sock_path = o.socket.clone().unwrap_or_else(|| home.join(format!("{}.sock", o.name)));
    if o.port.is_none() && sock_path.exists() {
        std::fs::remove_file(&sock_path).map_err(|e| format!("stale socket: {e}"))?;
    }

    // Token: an existing <name>.token survives restarts (stable identity for
    // clients and Caddy); minted on first serve. `--token ''` = auth off.
    let token_path = home.join(format!("{}.token", o.name));
    let token: Option<String> = match &o.token {
        Some(t) if t.is_empty() => None,
        Some(t) => Some(t.clone()),
        None => match std::fs::read_to_string(&token_path) {
            Ok(t) if !t.trim().is_empty() => Some(t.trim().to_string()),
            _ => {
                let t = harbor_core::random_token();
                std::fs::write(&token_path, &t).map_err(|e| format!("token file: {e}"))?;
                let _ = chmod(&token_path, 0o600);
                Some(t)
            }
        },
    };

    // The engine. One process, one database (D2): conservative memory default
    // so N berths coexist; printed so nobody is surprised.
    let con = duckdb_open(&o)?;
    let duckdb_version: String = con
        .query_row("SELECT version()", [], |r| r.get(0))
        .map_err(|e| format!("version: {e}"))?;
    // ATTACH is instance-wide, so once on the control connection covers the
    // whole pool — the cross-database-join case that justifies co-locating
    // files in one berth (D2/PLAN.md §4.3).
    let mut attached: Vec<String> = Vec::new();
    for spec in &o.attach {
        let (aname, rest) = spec
            .split_once('=')
            .ok_or_else(|| format!("--attach wants name=/path, got {spec:?}"))?;
        let aname = normalize(aname)?;
        let (apath, ro) = match rest.strip_suffix(":ro") {
            Some(p) => (p, " (READ_ONLY)"),
            None => (rest, ""),
        };
        con.execute_batch(&format!("ATTACH '{}' AS {aname}{ro}", apath.replace('\'', "''")))
            .map_err(|e| format!("attach {spec:?}: {e}"))?;
        attached.push(aname);
    }
    harbor_core::open_pool(con)?;

    let listen = match o.port {
        Some(port) => harbor_core::Listen::Tcp { bind: o.bind.clone(), port },
        None => harbor_core::Listen::Unix(sock_path.clone()),
    };
    let addr = harbor_core::start(listen, token, o.workers, o.log)?;
    if o.port.is_none() {
        let _ = chmod(&sock_path, 0o600);
    }

    // One identity, two consumers: GET /info (auth, live uptime spliced in by
    // the core) and the <name>.json sidecar `harbor ls` reads without dialing.
    let db_abs = std::fs::canonicalize(&o.db).unwrap_or(o.db.clone()).display().to_string();
    let mut databases = vec![o.name.clone()];
    databases.extend(attached);
    harbor_core::set_info(serde_json::json!({
        "protocolVersion": 1,
        "name": o.name,
        "harborVersion": VERSION,
        "duckdbVersion": duckdb_version,
        "database": db_abs,
        "databases": databases,
        "pid": std::process::id(),
        "mode": "binary",
        "grammar": false,
    }));

    let started_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    let info = serde_json::json!({
        "name": o.name,
        "pid": std::process::id(),
        "db": db_abs,
        "socket": if o.port.is_none() { Some(sock_path.display().to_string()) } else { None },
        "port": o.port,
        "harborVersion": VERSION,
        "duckdbVersion": duckdb_version,
        "startedAtMs": started_ms,
    });
    let json_path = home.join(format!("{}.json", o.name));
    let tmp = home.join(format!("{}.json.tmp", o.name));
    std::fs::write(&tmp, serde_json::to_vec_pretty(&info).unwrap())
        .and_then(|_| std::fs::rename(&tmp, &json_path))
        .map_err(|e| format!("registry json: {e}"))?;

    eprintln!(
        "harbor {VERSION}: berth {:?} serving {} on {} (duckdb {}, memory_limit {})",
        o.name,
        o.db.display(),
        addr,
        duckdb_version,
        o.memory_limit
    );

    // D9 ephemeral berths: no countable requests AND no live sessions for the
    // window → leave through the normal drain + CHECKPOINT door. Nothing
    // cleverer than this on purpose — no refcounts, no control sockets.
    if let Some(idle) = o.idle_exit {
        let tick = idle
            .div_f32(4.0)
            .min(Duration::from_secs(5))
            .max(Duration::from_millis(250));
        std::thread::spawn(move || loop {
            std::thread::sleep(tick);
            if harbor_core::idle_ms() >= idle.as_millis() as u64 && harbor_core::quiet() {
                eprintln!(
                    "harbor: idle {}s with no sessions — leaving the berth",
                    idle.as_secs()
                );
                let _ = harbor_core::stop();
                break;
            }
        });
    }

    // Blocks until harbor_stop / SIGTERM / idle-exit finishes drain + CHECKPOINT.
    let farewell = harbor_core::wait()?;
    if o.port.is_none() {
        let _ = std::fs::remove_file(&sock_path);
    }
    let _ = std::fs::remove_file(&json_path);
    let _ = std::fs::remove_file(&lock_path);
    eprintln!("harbor: berth {:?} closed ({farewell})", o.name);
    Ok(())
}

fn duckdb_open(o: &Opts) -> Result<harbor_core::duckdb::Connection, String> {
    let con = harbor_core::duckdb::Connection::open(&o.db).map_err(|e| format!("open {}: {e}", o.db.display()))?;
    con.execute_batch(&format!("SET memory_limit='{}'", o.memory_limit))
        .map_err(|e| format!("memory_limit: {e}"))?;
    if let Some(t) = o.threads {
        con.execute_batch(&format!("SET threads={t}")).map_err(|e| format!("threads: {e}"))?;
    }
    Ok(con)
}

// ---------------------------------------------------------------------------
// Fleet verbs — filesystem + probes, no engine
// ---------------------------------------------------------------------------

fn add(rest: Vec<String>) -> Result<(), String> {
    let o = parse_opts(rest.clone())?;
    let home = harbor_home()?;
    let log_dir = home.join("log");
    std::fs::create_dir_all(&log_dir).map_err(|e| format!("log dir: {e}"))?;
    let log = std::fs::File::create(log_dir.join(format!("{}.log", o.name)))
        .map_err(|e| format!("log file: {e}"))?;

    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let db_abs = std::fs::canonicalize(&o.db).unwrap_or(o.db.clone());
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("serve").arg(&db_abs);
    let mut pass = rest.into_iter().filter(|a| a != o.db.to_str().unwrap_or(""));
    // pass through every flag except the db positional
    let mut passed: Vec<String> = Vec::new();
    while let Some(a) = pass.next() {
        passed.push(a);
    }
    cmd.args(&passed);
    {
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;
        cmd.stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().map_err(|e| e.to_string())?))
            .stderr(Stdio::from(log))
            .process_group(0); // spawn, don't fork (D4): detached from our tty/session
    }
    let child = cmd.spawn().map_err(|e| format!("spawn: {e}"))?;

    let sock = o.socket.clone().unwrap_or_else(|| home.join(format!("{}.sock", o.name)));
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        let (up, at) = match o.port {
            None => (ready(&sock), sock.display().to_string()),
            Some(port) => (ready_tcp(&o.bind, port), format!("{}:{port}", o.bind)),
        };
        if up {
            println!("berth {:?} ready on {at} (db: {}, pid {})", o.name, db_abs.display(), child.id());
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!(
        "berth {:?} did not come up in 15s — see {}",
        o.name,
        log_dir.join(format!("{}.log", o.name)).display()
    ))
}

fn ready_tcp(bind: &str, port: u16) -> bool {
    let Ok(mut s) = std::net::TcpStream::connect((bind, port)) else { return false };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    if write!(s, "GET /ready HTTP/1.1\r\nHost: harbor\r\nConnection: close\r\n\r\n").is_err() {
        return false;
    }
    let mut buf = [0u8; 64];
    match s.read(&mut buf) {
        Ok(n) => String::from_utf8_lossy(&buf[..n]).contains(" 200 "),
        Err(_) => false,
    }
}

/// GET /ready over the socket — the only HTTP the fleet verbs speak.
fn ready(sock: &Path) -> bool {
    let Ok(mut s) = UnixStream::connect(sock) else { return false };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    if write!(s, "GET /ready HTTP/1.1\r\nHost: harbor\r\nConnection: close\r\n\r\n").is_err() {
        return false;
    }
    let mut buf = [0u8; 64];
    match s.read(&mut buf) {
        Ok(n) => String::from_utf8_lossy(&buf[..n]).contains(" 200 "),
        Err(_) => false,
    }
}

fn ls() -> Result<(), String> {
    let home = harbor_home()?;
    let mut socks: Vec<PathBuf> = std::fs::read_dir(&home)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "sock"))
        .collect();
    socks.sort();
    if socks.is_empty() {
        println!("no berths in {} (harbor add <db.duckdb>)", home.display());
        return Ok(());
    }
    println!("{:<20} {:<8} {:<8} {:<24} DB", "BERTH", "STATE", "PID", "DUCKDB");
    for sock in socks {
        let name = sock.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let state = if ready(&sock) { "ready" } else { "dead" };
        let (pid, duck, db) = std::fs::read_to_string(home.join(format!("{name}.json")))
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .map(|j| {
                (
                    j["pid"].as_u64().map(|p| p.to_string()).unwrap_or_default(),
                    j["duckdbVersion"].as_str().unwrap_or("-").to_string(),
                    j["db"].as_str().unwrap_or("-").to_string(),
                )
            })
            .unwrap_or_default();
        println!("{name:<20} {state:<8} {pid:<8} {duck:<24} {db}");
    }
    Ok(())
}

fn stop_berth(rest: Vec<String>, remove: bool) -> Result<(), String> {
    let name = rest.first().ok_or("which berth? (harbor ls)")?;
    let name = normalize(name)?;
    let home = harbor_home()?;
    let json_path = home.join(format!("{name}.json"));
    let sock = home.join(format!("{name}.sock"));

    if let Ok(s) = std::fs::read_to_string(&json_path) {
        if let Ok(j) = serde_json::from_str::<serde_json::Value>(&s) {
            if let Some(pid) = j["pid"].as_u64() {
                // SIGTERM is the contract: drain, CHECKPOINT, exit (core owns it).
                unsafe { libc::kill(pid as i32, libc::SIGTERM) };
                let deadline = Instant::now() + Duration::from_secs(35);
                while Instant::now() < deadline {
                    if unsafe { libc::kill(pid as i32, 0) } != 0 {
                        println!("berth {name:?} stopped (drained and checkpointed)");
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    } else if sock.exists() {
        return Err(format!("berth {name:?} has a socket but no registry json; kill it by pid"));
    } else if !remove {
        return Err(format!("no berth named {name:?}"));
    }

    if remove {
        for f in ["sock", "json", "token", "lock"] {
            let _ = std::fs::remove_file(home.join(format!("{name}.{f}")));
        }
        println!("berth {name:?} removed from the registry (database file untouched)");
    }
    Ok(())
}
