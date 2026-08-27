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
//!   harbor version                           print this binary's version
//!
//! The registry is the filesystem (D3): <name>.sock is the registration,
//! <name>.lock (flock) is the mutex, <name>.json is identity, <name>.token
//! is the credential. No daemon anywhere.

use std::io::{Read, Write};
#[cfg(unix)]
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
        "-V" | "--version" | "version" => {
            println!("harbor {}", env!("CARGO_PKG_VERSION"));
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
  harbor version                    print this binary's version (also -V)

serve/add options:
  --create            allow a database file that does not exist yet (the
                      positional is a PATH; without this flag a missing
                      file is an error, never a fresh database)
  --name <n>          berth name (default: db file stem)
  --socket <path>     unix socket (Unix only; default there: $HARBOR_HOME/<name>.sock)
  --port <p>          listen on TCP 127.0.0.1:<p> instead of a unix socket
  --bind <addr>       TCP bind address (with --port; default 127.0.0.1)
  --token <t>         bearer token ('' disables auth; default: <name>.token,
                      minted on first serve)
  --workers <n>       executor pool size (default 6)
  --memory-limit <s>  DuckDB memory_limit (default 2GB — fleet-safe, D2)
  --threads <n>       DuckDB threads (default: DuckDB's own)
  --idle-exit <d>     drain, CHECKPOINT and exit after <d> (e.g. 90s, 10m) with
                      no requests and no live sessions (D9 ephemeral berths)
  --init <sql>        run SQL at boot, before serving (repeatable) — the door
                      for extensions: --init 'LOAD ui; CALL start_ui_server()'
  --unsigned          allow unsigned extensions (open-time only; needed to
                      LOAD an unsigned build, e.g. the duckdb-ui 2.0 fork)
  --sealed            lock the berth to SQL on its own database: no host file
                      access (read_csv/COPY), no community extensions. For a
                      berth an untrusted caller can reach
  --statement-timeout <d>  hard deadline ceiling per statement (e.g. 30s); a
                      request may ask for a shorter timeout, but not a longer one
  --max-temp-size <s>  cap spill-to-disk (e.g. 10GB; default: DuckDB's own)
  --log               log requests to stderr
";

struct Opts {
    db: PathBuf,
    create: bool,
    name: String,
    socket: Option<PathBuf>,
    port: Option<u16>,
    bind: String,
    token: Option<String>, // None = use/mint <name>.token; Some("") = auth off
    workers: usize,
    memory_limit: String,
    threads: Option<u32>,
    idle_exit: Option<Duration>,
    init: Vec<String>,
    log: bool,
    unsigned: bool,
    sealed: bool,
    statement_timeout: Option<Duration>,
    max_temp_size: Option<String>,
}

/// "90s", "10m", "2h", or bare seconds.
fn parse_duration(s: &str) -> Result<Duration, String> {
    // Split off a trailing alphabetic unit. `trim_end_matches` works in whole
    // chars, so a multibyte unit (`5µs`, `5é`) can't land `split_at` inside a
    // UTF-8 char and panic — it just fails the unit match below.
    let num = s.trim_end_matches(char::is_alphabetic);
    let unit = &s[num.len()..];
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
        create: false,
        name: String::new(),
        socket: None,
        port: None,
        bind: "127.0.0.1".into(),
        token: None,
        workers: harbor::DEFAULT_MAX_INFLIGHT,
        memory_limit: "2GB".into(),
        threads: None,
        idle_exit: None,
        init: Vec::new(),
        log: false,
        unsigned: false,
        sealed: false,
        statement_timeout: None,
        max_temp_size: None,
    };
    let mut named: Option<String> = None;
    while let Some(a) = it.next() {
        let mut take = |what: &str| it.next().ok_or(format!("--{what} needs a value"));
        match a.as_str() {
            "--create" => o.create = true,
            "--name" => named = Some(take("name")?),
            "--socket" => o.socket = Some(PathBuf::from(take("socket")?)),
            "--port" => o.port = Some(take("port")?.parse().map_err(|_| "bad --port")?),
            "--bind" => o.bind = take("bind")?,
            "--token" => o.token = Some(take("token")?),
            "--workers" => o.workers = take("workers")?.parse().map_err(|_| "bad --workers")?,
            "--memory-limit" => o.memory_limit = take("memory-limit")?,
            "--threads" => o.threads = Some(take("threads")?.parse().map_err(|_| "bad --threads")?),
            "--idle-exit" => o.idle_exit = Some(parse_duration(&take("idle-exit")?)?),
            "--init" => o.init.push(take("init")?),
            "--log" => o.log = true,
            "--unsigned" => o.unsigned = true,
            "--sealed" => o.sealed = true,
            "--statement-timeout" => {
                o.statement_timeout = Some(parse_duration(&take("statement-timeout")?)?)
            }
            "--max-temp-size" => o.max_temp_size = Some(take("max-temp-size")?),
            _ if db.is_none() && !a.starts_with('-') => db = Some(PathBuf::from(a)),
            other => return Err(format!("unexpected argument: {other}")),
        }
    }
    o.db = db.ok_or("no database file given")?;
    // The positional is a PATH — `harbor add medlabs` from ~/x names the
    // file ~/x/medlabs, and silently creating a fresh database there (then
    // serving it under the very name clients trust) put an empty impostor
    // in front of real data once already. Creation is opt-in, loudly.
    if !o.db.exists() && !o.create {
        return Err(format!(
            "database file not found: {} (the argument is a path, not a berth name; pass --create to make a new database here)",
            o.db.display()
        ));
    }
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
            let h = std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .map_err(|_| "neither $HOME nor %USERPROFILE% is set")?;
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
    #[cfg(unix)]
    {
    use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

/// Claim a berth name for this process's lifetime. Unix uses flock so the
/// inode can remain forever; Windows opens the file with no sharing, which is
/// the native equivalent and releases automatically when the process exits.
fn claim_lock(path: &Path) -> Result<std::fs::File, String> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let lock = std::fs::File::create(path).map_err(|e| format!("lock: {e}"))?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(format!("berth lock {} is already claimed", path.display()));
        }
        Ok(lock)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .share_mode(0)
            .open(path)
            .map_err(|_| format!("berth lock {} is already claimed", path.display()))
    }
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
    let lock = claim_lock(&lock_path).map_err(|_| {
        format!("berth {:?} is already claimed in {}", o.name, home.display())
    })?;
    std::mem::forget(lock); // hold the flock until the process exits

    let sock_path = o.socket.clone().unwrap_or_else(|| home.join(format!("{}.sock", o.name)));
    if cfg!(unix) && o.port.is_none() && sock_path.exists() {
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
                let t = harbor::random_token();
                std::fs::write(&token_path, &t).map_err(|e| format!("token file: {e}"))?;
                let _ = chmod(&token_path, 0o600);
                Some(t)
            }
        },
    };

    // A statement deadline ceiling, if asked. The engine reads
    // HARBOR_STATEMENT_TIMEOUT_MS per request and clamps a requested timeout
    // to it; setting the variable here, before serving, turns the CLI flag into
    // that hard cap. Left unset by default on purpose: harbor streams
    // minute-long analytical queries, so a blanket deadline would break
    // correct programs.
    if let Some(d) = o.statement_timeout {
        // SAFETY: single-threaded here — start() has not spawned the workers.
        unsafe { std::env::set_var("HARBOR_STATEMENT_TIMEOUT_MS", d.as_millis().to_string()) };
    }

    // The engine. One process, one database (D2): conservative memory default
    // so N berths coexist; printed so nobody is surprised.
    let con = duckdb_open(&o)?;
    let duckdb_version: String = con
        .query_row("SELECT version()", [], |r| r.get(0))
        .map_err(|e| format!("version: {e}"))?;
    // Boot SQL runs on the control connection before the pool forms, so its
    // effects (LOAD ui, settings, secrets) are instance-wide and in place
    // before the first request. This one flag is the whole extension story:
    // ui, quack, httpfs — harbor stays agnostic about all of them.
    for sql in &o.init {
        con.execute_batch(sql).map_err(|e| format!("--init {sql:?}: {e}"))?;
    }
    harbor::open_pool(con)?;

    #[cfg(unix)]
    let listen = match o.port {
        Some(port) => harbor::Listen::Tcp { bind: o.bind.clone(), port },
        None => harbor::Listen::Unix(sock_path.clone()),
    };
    // Windows has no Unix-domain fleet face. Port zero asks the OS for a free
    // loopback port; the actual port is recorded in the sidecar below.
    #[cfg(windows)]
    let listen = harbor::Listen::Tcp { bind: o.bind.clone(), port: o.port.unwrap_or(0) };
    let addr = harbor::start(listen, token, o.workers, o.log)?;
    let tcp = o.port.is_some() || cfg!(windows);
    let bound_port = if tcp {
        addr.parse::<std::net::SocketAddr>().ok().map(|a| a.port()).or(o.port)
    } else {
        None
    };
    if !tcp {
        let _ = chmod(&sock_path, 0o600);
    }

    // One identity, two consumers: GET /info (auth, live uptime spliced in by
    // the core) and the <name>.json sidecar `harbor ls` reads without dialing.
    let db_abs = std::fs::canonicalize(&o.db).unwrap_or(o.db.clone()).display().to_string();
    harbor::set_info(serde_json::json!({
        "protocolVersion": 1,
        "name": o.name,
        "harborVersion": VERSION,
        "duckdbVersion": duckdb_version,
        "database": db_abs,
        "databases": [o.name],
        "pid": std::process::id(),
        "grammar": false,
        // Pilot uses this to pulse comfortably inside the actual idle window.
        // null means this berth is permanent and needs no prompt heartbeat.
        "idleExitMs": o.idle_exit.map(|d| d.as_millis() as u64),
    }));

    let started_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    let info = serde_json::json!({
        "name": o.name,
        "pid": std::process::id(),
        "db": db_abs,
        "socket": if tcp { None } else { Some(sock_path.display().to_string()) },
        "port": bound_port,
        "bind": if tcp { Some(o.bind.clone()) } else { None },
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
            if harbor::idle_ms() >= idle.as_millis() as u64 && harbor::quiet() {
                eprintln!(
                    "harbor: idle {}s with no sessions — leaving the berth",
                    idle.as_secs()
                );
                let _ = harbor::stop();
                break;
            }
        });
    }

    // Blocks until harbor_stop / SIGTERM / idle-exit finishes drain + CHECKPOINT.
    let farewell = harbor::wait()?;
    if !tcp {
        let _ = std::fs::remove_file(&sock_path);
    }
    let _ = std::fs::remove_file(&json_path);
    // The lock file stays. flock releases with the process, and a lock file
    // with no holder is harmless — but unlinking it while another claimant
    // has the old inode open lets a third claimant create a fresh inode and
    // flock it too: two winners, one database. Never unlink a lock file.
    eprintln!("harbor: berth {:?} closed ({farewell})", o.name);
    Ok(())
}

fn duckdb_open(o: &Opts) -> Result<harbor::duckdb::Connection, String> {
    use harbor::duckdb::{Config, Connection};
    // These four settings can only be chosen when the connection is opened,
    // not with a later SET, so they must reach the Connection here:
    //   --unsigned  allow_unsigned_extensions — the one door for loading an
    //               unsigned build (the duckdb-ui 2.0 fork, DUCKDB-UI-V2-
    //               COMPAT.md) via --init 'LOAD ui; CALL start_ui_server()'.
    //   --sealed    enable_external_access=false + allow_community_extensions
    //               =false — shrinks a token from host access (read_csv of any
    //               file, COPY TO disk, community native code) to a credential
    //               for this one database. For a berth an untrusted caller can
    //               reach. Default off: read_csv/COPY are core data workflows
    //               (the test fixtures themselves load CSV), so the safe edge
    //               is the operator's to draw, like TLS (D6).
    // Signed-only, full-access is the default; each is opt-in.
    let con = if o.unsigned || o.sealed {
        let mut config = Config::default();
        if o.unsigned {
            config = config.allow_unsigned_extensions().map_err(|e| format!("config: {e}"))?;
        }
        if o.sealed {
            config = config
                .with("enable_external_access", "false")
                .and_then(|c| c.with("allow_community_extensions", "false"))
                .map_err(|e| format!("config: {e}"))?;
        }
        Connection::open_with_flags(&o.db, config)
            .map_err(|e| format!("open {}: {e}", o.db.display()))?
    } else {
        Connection::open(&o.db).map_err(|e| format!("open {}: {e}", o.db.display()))?
    };
    con.execute_batch(&format!("SET memory_limit='{}'", o.memory_limit))
        .map_err(|e| format!("memory_limit: {e}"))?;
    if let Some(t) = o.threads {
        con.execute_batch(&format!("SET threads={t}")).map_err(|e| format!("threads: {e}"))?;
    }
    // A ceiling on spill-to-disk, so one large query cannot fill the host
    // disk. Default unset (DuckDB's own 90%-of-free); the operator caps it.
    if let Some(s) = &o.max_temp_size {
        con.execute_batch(&format!("SET max_temp_directory_size='{s}'"))
            .map_err(|e| format!("max_temp_size: {e}"))?;
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
    let log_path = log_dir.join(format!("{}.log", o.name));
    // Append, never truncate: a berth may already be serving under this name
    // (this add is then the loser of a race, or a mistake), and File::create
    // would wipe the live process's history out from under it — the log an
    // operator needs most at exactly the moment it went missing.
    let log = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&log_path)
        .map_err(|e| format!("log file: {e}"))?;

    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    // canonicalize fails for a --create target that doesn't exist yet;
    // absolutize by hand so the detached child (and its sidecar) never
    // depend on inheriting this cwd.
    let db_abs = std::fs::canonicalize(&o.db).unwrap_or_else(|_| {
        std::env::current_dir().map(|d| d.join(&o.db)).unwrap_or_else(|_| o.db.clone())
    });
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("serve").arg(&db_abs);
    // Pass every argument through except the db positional — the first
    // occurrence only, so a flag value that happens to equal the path
    // survives.
    let mut skipped_db = false;
    let passed: Vec<String> = rest
        .into_iter()
        .filter(|a| {
            if !skipped_db && a == o.db.to_str().unwrap_or("") {
                skipped_db = true;
                false
            } else {
                true
            }
        })
        .collect();
    cmd.args(&passed);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;
        cmd.stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().map_err(|e| e.to_string())?))
            .stderr(Stdio::from(log))
            .process_group(0); // spawn, don't fork (D4): detached from our tty/session
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use std::process::Stdio;
        // CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS: the child owns no
        // console and survives the short-lived `harbor add` launcher.
        cmd.stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().map_err(|e| e.to_string())?))
            .stderr(Stdio::from(log))
            .creation_flags(0x0000_0208);
    }
    let mut child = cmd.spawn().map_err(|e| format!("spawn: {e}"))?;

    #[cfg(unix)]
    let sock = o.socket.clone().unwrap_or_else(|| home.join(format!("{}.sock", o.name)));
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        // The child dying is an answer, not a timeout: it lost the flock to a
        // berth already serving, or failed outright. Without this check the
        // existing berth answers /ready and this add reports success with the
        // dead child's pid — which an orchestrator then records and signals.
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!(
                "the berth did not start ({status}); a berth may already serve this name — \
                 see harbor ls, or {}",
                log_path.display()
            ));
        }
        #[cfg(windows)]
        let registered = registered_tcp(&home, &o.name);
        #[cfg(unix)]
        let (up, at) = match o.port {
            None => (ready(&sock), sock.display().to_string()),
            Some(port) => (ready_tcp(&o.bind, port), format!("{}:{port}", o.bind)),
        };
        #[cfg(windows)]
        let (up, at) = match registered {
            Some((bind, port)) => (ready_tcp(&bind, port), format!("{bind}:{port}")),
            None => (false, "a dynamically assigned TCP port".to_string()),
        };
        if up {
            // Ready is necessary but not sufficient: on a name collision the
            // EXISTING berth answers this probe instantly, before our child
            // has even lost its flock. The sidecar json names who actually
            // serves — believe it, not the probe. A missing or mismatched
            // json means our child has not written it (keep polling) or never
            // will (the try_wait above reports that next lap).
            let served_by = std::fs::read_to_string(home.join(format!("{}.json", o.name)))
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .and_then(|j| j["pid"].as_u64());
            if served_by == Some(u64::from(child.id())) {
                println!(
                    "berth {:?} ready on {at} (db: {}, pid {})",
                    o.name,
                    db_abs.display(),
                    child.id()
                );
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!("berth {:?} did not come up in 15s — see {}", o.name, log_path.display()))
}

fn ready_tcp(bind: &str, port: u16) -> bool {
    let bind = match bind {
        "0.0.0.0" | "::" => "127.0.0.1",
        other => other,
    };
    let Ok(mut s) = std::net::TcpStream::connect((bind, port)) else { return false };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    probe_200(&mut s)
}

#[cfg(windows)]
fn registered_tcp(home: &Path, name: &str) -> Option<(String, u16)> {
    let text = std::fs::read_to_string(home.join(format!("{name}.json"))).ok()?;
    let j: serde_json::Value = serde_json::from_str(&text).ok()?;
    let port = u16::try_from(j["port"].as_u64()?).ok()?;
    let bind = j["bind"].as_str().unwrap_or("127.0.0.1").to_string();
    Some((bind, port))
}

/// GET /ready over the socket — the only HTTP the fleet verbs speak.
#[cfg(unix)]
fn ready(sock: &Path) -> bool {
    let Ok(mut s) = UnixStream::connect(sock) else { return false };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    probe_200(&mut s)
}

#[cfg(windows)]
fn ready(_sock: &Path) -> bool {
    false
}

#[cfg(windows)]
fn shutdown_tcp(bind: &str, port: u16, token: Option<&str>) -> bool {
    let bind = match bind {
        "0.0.0.0" | "::" => "127.0.0.1",
        other => other,
    };
    let Ok(mut s) = std::net::TcpStream::connect((bind, port)) else { return false };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let mut request =
        "DELETE /shutdown HTTP/1.1\r\nHost: harbor\r\nConnection: close\r\n".to_string();
    if let Some(token) = token {
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    request.push_str("\r\n");
    if s.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 128];
    match s.read(&mut buf) {
        Ok(n) => String::from_utf8_lossy(&buf[..n]).contains(" 202 "),
        Err(_) => false,
    }
}

/// Positive proof that a berth is gone: its `<name>.lock` exists and we can
/// take the exclusive flock the live berth holds for its whole life (serve).
/// Only a *definite* death suppresses a stop/rm signal — if the lock file is
/// missing or unreadable we return false and fall through to the old behaviour,
/// so a live berth is never left unstoppable. Unlike `GET /ready` this needs no
/// response from the berth, so a busy one is correctly seen as alive, not dead.
fn berth_dead(lock_path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let Ok(f) = std::fs::OpenOptions::new().read(true).open(lock_path) else {
            return false; // unknown, not proven dead
        };
        // If we acquire it, nobody holds it → the process is gone. Dropping
        // `f` releases the lock we just took; never unlink it (see serve).
        unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // The live process holds this file with share_mode(0). Successfully
        // opening it the same way is positive proof that process is gone.
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(lock_path)
            .is_ok()
    }
}

fn probe_200(s: &mut (impl Read + Write)) -> bool {
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
    // Keyed off the sidecar json — the file that exists precisely so ls can
    // read identity without dialing — because a --port berth has no socket
    // and was invisible when this listed *.sock.
    let mut jsons: Vec<PathBuf> = std::fs::read_dir(&home)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    jsons.sort();
    if jsons.is_empty() {
        println!("no berths in {} (harbor add <db.duckdb>)", home.display());
        return Ok(());
    }
    println!("{:<20} {:<8} {:<8} {:<24} {:<22} DB", "BERTH", "STATE", "PID", "DUCKDB", "ADDRESS");
    for path in jsons {
        let name = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let j = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .unwrap_or_default();
        let state = match (j["socket"].as_str(), j["port"].as_u64()) {
            (Some(sock), _) if ready(Path::new(sock)) => "ready",
            (None, Some(port)) if ready_tcp("127.0.0.1", port as u16) => "ready",
            _ => "dead",
        };
        // How to dial it — the answer "ready" begs for. Socket berths show the
        // path (~-shortened), TCP berths bind:port; pilot accepts either form.
        let addr = match (j["socket"].as_str(), j["port"].as_u64()) {
            (Some(sock), _) => tilde(sock),
            (None, Some(port)) => format!("{}:{port}", j["bind"].as_str().unwrap_or("127.0.0.1")),
            _ => "-".to_string(),
        };
        let pid = j["pid"].as_u64().map(|p| p.to_string()).unwrap_or_default();
        let duck = j["duckdbVersion"].as_str().unwrap_or("-").to_string();
        let db = j["db"].as_str().unwrap_or("-").to_string();
        println!("{name:<20} {state:<8} {pid:<8} {duck:<24} {addr:<22} {db}");
    }
    Ok(())
}

fn tilde(path: &str) -> String {
    match std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        Ok(h) if path.starts_with(&h) => format!("~{}", &path[h.len()..]),
        _ => path.to_string(),
    }
}

fn stop_berth(rest: Vec<String>, remove: bool) -> Result<(), String> {
    let name = rest.first().ok_or("which berth? (harbor ls)")?;
    let name = normalize(name)?;
    let home = harbor_home()?;
    let json_path = home.join(format!("{name}.json"));
    let sock = home.join(format!("{name}.sock"));

    let lock_path = home.join(format!("{name}.lock"));
    if let Ok(s) = std::fs::read_to_string(&json_path) {
        if let Ok(j) = serde_json::from_str::<serde_json::Value>(&s) {
            if berth_dead(&lock_path) {
                // Proven dead: no process holds the flock, so the pid recorded
                // in the json is stale and the OS may have recycled it to an
                // unrelated process. Signalling it would be signalling a
                // stranger — so don't. `rm` still cleans the registry below.
                let pid = j["pid"].as_u64().map(|p| p.to_string()).unwrap_or_default();
                if !remove {
                    return Err(format!(
                        "berth {name:?} is not running (stale pid {pid}); nothing to signal. \
                         Use `harbor rm {name}` to clear the registry entry."
                    ));
                }
            } else if let Some(pid) = j["pid"].as_u64() {
                #[cfg(unix)]
                {
                // SIGTERM is the contract: drain, CHECKPOINT, exit (core owns it).
                unsafe { libc::kill(pid as i32, libc::SIGTERM) };
                let deadline = Instant::now() + Duration::from_secs(35);
                let mut died = false;
                while Instant::now() < deadline {
                    if unsafe { libc::kill(pid as i32, 0) } != 0 {
                        died = true;
                        println!("berth {name:?} stopped (drained and checkpointed)");
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                if !died {
                    // Exiting 0 here would be a lie an operator scripts on.
                    // And removing registry files under a live berth is worse:
                    // deleting its .lock lets a future serve create a fresh
                    // inode, flock it, and claim the same name — two berths,
                    // one database, the exact thing the mutex prevents.
                    return Err(format!(
                        "berth {name:?} (pid {pid}) is still running 35s after SIGTERM — \
                         nothing was removed. Escalate by hand if you mean it: kill -9 {pid}"
                    ));
                }
                }
                #[cfg(windows)]
                {
                    let (bind, port) = registered_tcp(&home, &name)
                        .ok_or_else(|| format!("berth {name:?} has no registered TCP address"))?;
                    let token = std::fs::read_to_string(home.join(format!("{name}.token")))
                        .ok()
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty());
                    if !shutdown_tcp(&bind, port, token.as_deref()) {
                        return Err(format!(
                            "berth {name:?} (pid {pid}) refused the graceful shutdown request"
                        ));
                    }
                    let deadline = Instant::now() + Duration::from_secs(35);
                    while Instant::now() < deadline && !berth_dead(&lock_path) {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    if !berth_dead(&lock_path) {
                        return Err(format!(
                            "berth {name:?} (pid {pid}) is still running 35s after shutdown — \
                             nothing was removed. Escalate with Task Manager if you mean it."
                        ));
                    }
                    println!("berth {name:?} stopped (drained and checkpointed)");
                }
            }
        }
    } else if sock.exists() {
        return Err(format!("berth {name:?} has a socket but no registry json; kill it by pid"));
    } else if !remove {
        return Err(format!("no berth named {name:?}"));
    }

    if remove {
        // The lock file stays (see serve): a stale one with no holder is
        // harmless, and unlinking one is never safe.
        for f in ["sock", "json", "token"] {
            let _ = std::fs::remove_file(home.join(format!("{name}.{f}")));
        }
        println!("berth {name:?} removed from the registry (database file untouched)");
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::berth_dead;
    use std::os::fd::AsRawFd;

    #[test]
    fn berth_dead_reads_the_flock() {
        // A unique temp path so parallel test runs never collide.
        let dir = std::env::temp_dir();
        let path = dir.join(format!("harbor-test-{}.lock", std::process::id()));
        let _ = std::fs::remove_file(&path);

        // No lock file at all → unknown, not proven dead (never strand a live
        // berth just because its lock file is missing).
        assert!(!berth_dead(&path), "missing lock file is not proof of death");

        // A lock file that nobody holds → the berth is gone → proven dead.
        let held = std::fs::File::create(&path).unwrap();
        assert!(berth_dead(&path), "an unheld lock file means the berth is gone");

        // While a process holds the exclusive flock (as a live berth does), the
        // berth is not dead — even though it answers nothing here.
        assert_eq!(
            unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0,
            "test should be able to take the lock"
        );
        assert!(!berth_dead(&path), "a held lock file means the berth is alive");

        unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_UN) };
        let _ = std::fs::remove_file(&path);
    }
}
