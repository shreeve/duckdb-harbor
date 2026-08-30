//! harbor — DuckDB wearing a server.
//!
//! One binary, two jobs (PLAN.md D8): `serve` embeds DuckDB and owns one
//! database file; the fleet verbs (`start`, `show`, `stop`, `forget`, `doctor`)
//! manage the berths of ~/.local/state/harbor/runtime from outside, linking no
//! engine code paths at all.
//!
//!   harbor serve  db.duckdb [--name n] [--socket p | --port p] [--token t]
//!   harbor start  <name|db.duckdb>           spawn a detached berth, wait ready
//!   harbor show   [name]                     the fleet, or one berth in detail
//!   harbor stop   <name>                     SIGTERM → drain, CHECKPOINT
//!   harbor forget <name>                     stop + clear registry (never the db)
//!   harbor doctor                            what nothing else has a moment to see
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

mod doctor;

// Paths, names, permissions and durations live in harbor-common so pilot and
// ducktable cannot drift from them. What stays here is what only a server
// does: claiming a berth, and creating the directory it claims in.
use harbor_common::lifetime::parse_duration;
use harbor_common::perms::{chmod, write_private};
use harbor_common::normalize;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match args.split_first() {
        Some((c, r)) => (c.as_str(), r.to_vec()),
        // Bare `harbor` shows the fleet — systemctl's precedent, and the job
        // this binary does: the client connects you, the manager reports to
        // you. `harbor help` is still the route to the verb list.
        None => ("show", Vec::new()),
    };
    let result = match cmd {
        "serve" => serve(rest),
        "start" => start(rest),
        "show" => show(rest),
        "stop" => stop_berth(rest, false),
        "forget" => stop_berth(rest, true),
        "doctor" => doctor_cmd(rest),
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
  harbor show   [name]              the fleet, or one berth in detail
  harbor start  <name|db.duckdb>    spawn a detached berth, wait until ready
  harbor stop   <name>              SIGTERM the berth: drain, CHECKPOINT, exit
  harbor forget <name>              stop it and drop it (never the database)
  harbor doctor                     check the config for what nothing else sees
  harbor serve  <db.duckdb> [opts]  own a database, serve it (foreground)
  harbor version                    print this binary's version (also -V)

Bare `harbor` is `harbor show`.

A bare word always names a configured berth, never a file — which is what
stops `harbor start medlabs`, run from the wrong directory, from meaning the
file ./medlabs. A path carries a / or ends in .duckdb.

serve/start options (a config entry may set any of these; a flag here wins):
  --create            allow a database file that does not exist yet (the
                      positional is a PATH; without this flag a missing
                      file is an error, never a fresh database)
  --name <n>          berth name (default: db file stem)
  --socket <path>     unix socket (Unix only; default there: $HARBOR_HOME/runtime/<name>.sock)
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
    // The positional is a PATH — `harbor start medlabs` from ~/x names the
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


/// The runtime dir, created and tightened before use.
///
/// Harbor is the only thing that writes here, so it is the only thing that
/// creates it. Both this and the state root are chmod'd on every run, not
/// just at creation: they hold sockets and token files, which are the local
/// access control, and a directory made earlier by hand or under a sloppy
/// umask must not be allowed to stay world-listable.
///
/// The config root is tightened only if it already exists. Nothing of
/// harbor's lives there, and a server should not conjure a config directory.
fn harbor_home() -> Result<PathBuf, String> {
    let run = harbor_common::runtime_dir()?;
    harbor_common::perms::ensure_private_dir(&run)?;
    if let Ok(state) = harbor_common::state_root() {
        let _ = chmod(&state, 0o700);
    }
    if let Ok(cfg) = harbor_common::config_root() {
        if cfg.exists() {
            let _ = chmod(&cfg, 0o700);
        }
    }
    Ok(run)
}

/// Claim a berth name for this process's lifetime. Unix uses flock so the
/// inode can remain forever; Windows opens the file with no sharing, which is
/// the native equivalent and releases automatically when the process exits.
///
/// Two properties beyond taking the lock, both needed now that other verbs
/// touch these files routinely:
///
/// **It retries briefly.** `harbor show` flocks every lock file to test it and
/// `forget` takes one to unlink it, each for microseconds. Without a retry,
/// a `start` that lands in one of those windows fails with "already claimed"
/// about a berth nobody is running.
///
/// **It revalidates the inode.** `forget` can unlink a lock file, so the path
/// this opened may no longer be the path this holds. If they differ, another
/// claimant could take the fresh inode and win the same name — two servers,
/// one database. Comparing dev+ino after the lock is the dpkg/apt protocol,
/// and it is what makes unlinking a lock safe at all.
fn claim_lock(path: &Path) -> Result<std::fs::File, String> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            let lock = std::fs::File::create(path).map_err(|e| format!("lock: {e}"))?;
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                // Held. Is it still the file at this path?
                let held = lock.metadata().map_err(|e| e.to_string())?;
                match std::fs::metadata(path) {
                    Ok(now) if now.dev() == held.dev() && now.ino() == held.ino() => {
                        return Ok(lock);
                    }
                    // Unlinked or replaced under us: drop it and take the new one.
                    _ if Instant::now() < deadline => continue,
                    _ => return Err(format!("berth lock {} keeps changing", path.display())),
                }
            }
            if Instant::now() >= deadline {
                return Err(format!("berth lock {} is already claimed", path.display()));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .share_mode(0)
                .open(path)
            {
                Ok(f) => return Ok(f),
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Err(_) => {
                    return Err(format!("berth lock {} is already claimed", path.display()));
                }
            }
        }
    }
}

/// Unlink a lock file, but only while holding it.
///
/// The rule everywhere else in this file is *never unlink a lock*, because
/// unlinking one another claimant has open lets a third create a fresh inode
/// and flock that: two winners, one database. Holding the lock across the
/// unlink is what suspends that rule safely — a concurrent `serve` either
/// loses the flock and waits, or wins after the unlink and revalidates onto
/// the new inode. Returns whether anything was removed.
fn unlink_lock_if_free(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let Ok(f) = std::fs::OpenOptions::new().read(true).open(path) else { return false };
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return false; // someone is serving under this name
        }
        let removed = std::fs::remove_file(path).is_ok();
        drop(f);
        removed
    }
    #[cfg(not(unix))]
    {
        std::fs::remove_file(path).is_ok()
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
                // 0600 at creation, not after: `fs::write` applies the umask,
                // so the plain form publishes the bearer token at 0644 for the
                // instant before the chmod. The chmod stays as well, because
                // `mode()` only governs a file this call creates and an older
                // token file may already be sitting there with looser bits.
                write_private(&token_path, &t).map_err(|e| format!("token file: {e}"))?;
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
    // The ATTACHED CATALOG NAMES, which is what a client must qualify its
    // queries with — not this berth's name, which is an operator's label and
    // routinely differs (berth "tpdemo" serving demo.duckdb has catalog
    // "demo"). Reporting the berth name here made every catalog query a
    // client wrote filter on a database that does not exist, and the failure
    // was silent: an empty schema list and no error to explain it.
    //
    // Read once, here, because this runs AFTER the boot SQL above — so an
    // ATTACH in --init is included. A later runtime ATTACH is not; /info is a
    // pure in-memory read and must stay one, since it has to keep answering
    // when every worker is busy.
    let databases: Vec<String> = con
        .prepare("SELECT database_name FROM duckdb_databases() WHERE NOT internal ORDER BY database_name")
        .and_then(|mut stmt| {
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<Result<Vec<String>, _>>()
        })
        .unwrap_or_default();
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
    // the core) and the <name>.json sidecar `harbor show` reads without dialing.
    let db_abs = std::fs::canonicalize(&o.db).unwrap_or(o.db.clone()).display().to_string();
    harbor::set_info(serde_json::json!({
        "protocolVersion": 1,
        "name": o.name,
        "harborVersion": VERSION,
        "duckdbVersion": duckdb_version,
        "database": db_abs,
        "databases": databases,
        "pid": std::process::id(),
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

fn spawn_detached(rest: Vec<String>) -> Result<(), String> {
    let o = parse_opts(rest.clone())?;
    let home = harbor_home()?;
    let log_dir = home.join("log");
    std::fs::create_dir_all(&log_dir).map_err(|e| format!("log dir: {e}"))?;
    let log_path = log_dir.join(format!("{}.log", o.name));
    // Append, never truncate: a berth may already be serving under this name
    // (this start is then the loser of a race, or a mistake), and File::create
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
        // console and survives the short-lived `harbor start` launcher.
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
        // existing berth answers /ready and this start reports success with the
        // dead child's pid — which an orchestrator then records and signals.
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!(
                "the berth did not start ({status}); a berth may already serve this name — \
                 see harbor show, or {}",
                log_path.display()
            ));
        }
        #[cfg(unix)]
        let up = match o.port {
            None => ready(&sock),
            Some(port) => ready_tcp(&o.bind, port),
        };
        // On Windows the child picks its own port, so the sidecar is the only
        // place that says where to knock.
        #[cfg(windows)]
        let up = match registered_tcp(&home, &o.name) {
            Some((bind, port)) => ready_tcp(&bind, port),
            None => false,
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
                // The receipt is the fleet. A one-line "ready on <socket>"
                // restated the flags you just typed; the table answers the
                // question you actually had, which is what changed.
                return show(Vec::new());
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

/// The config, or a clear reason why not.
///
/// `load_or_empty` is wrong for the fleet verbs: a refused or invalid config
/// returns *empty*, which would silently reclassify every configured berth as
/// unmanaged and make every stopped one vanish. A confidently wrong table is
/// worse than an error.
fn load_config() -> Result<harbor_common::config::FileConfig, String> {
    match harbor_common::config::load() {
        Ok(c) => Ok(c),
        Err(harbor_common::config::Error::Missing(_)) => Ok(Default::default()),
        Err(e) => Err(e.to_string()),
    }
}

/// Levenshtein — just enough for "did you mean".
fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Unknown name: say what exists, and guess only when the guess is close.
///
/// Deliberately never *runs* the guess the way git's help.autocorrect can. Do
/// not act on a guess that starts a server.
fn unknown_berth(cfg: &harbor_common::config::FileConfig, name: &str) -> String {
    let known: Vec<&str> = cfg.berths().into_iter().map(|(n, _)| n).collect();
    let mut msg = format!("no berth named {name:?} in your config");
    if let Some(near) = known.iter().find(|k| edit_distance(k, name) <= 2) {
        msg.push_str(&format!("\n        did you mean: {near}?"));
    }
    match known.is_empty() {
        true => msg.push_str("\n        nothing is configured yet — harbor start <db.duckdb>"),
        false => msg.push_str(&format!("\n        configured: {}", known.join(", "))),
    }
    msg.push_str("\n        (a bare word always names a berth; a path needs a / or .duckdb)");
    msg
}

/// A config entry, rendered as `serve` flags.
///
/// Config is translated into argv rather than read twice, so `serve` stays
/// flag-complete: a systemd unit, a container, and the test harness never need
/// a config file at all.
fn entry_args(
    c: &harbor_common::config::Connection,
    d: &harbor_common::config::Defaults,
) -> Result<Vec<String>, String> {
    let mut v: Vec<String> = Vec::new();
    // A human typing `harbor start` asked for a server, so absent means
    // persistent — but an entry naming its own idle-exit wins over that.
    let life = harbor_common::lifetime::resolve(
        Default::default(),
        c.idle_exit.as_deref(),
        d.idle_exit.as_deref(),
        harbor_common::Summoner::Operator,
    )?;
    v.extend(life.to_args());

    let (threads, workers, port) = (
        c.threads.or(d.threads).map(|n| n.to_string()),
        c.workers.or(d.workers).map(|n| n.to_string()),
        c.port.map(|n| n.to_string()),
    );
    let pairs: [(&str, Option<&str>); 7] = [
        ("memory-limit", c.memory_limit.as_deref().or(d.memory_limit.as_deref())),
        ("statement-timeout", c.statement_timeout.as_deref()),
        ("max-temp-size", c.max_temp_size.as_deref()),
        ("bind", c.bind.as_deref()),
        ("threads", threads.as_deref()),
        ("workers", workers.as_deref()),
        ("port", port.as_deref()),
    ];
    for (flag, val) in pairs {
        if let Some(x) = val {
            v.push(format!("--{flag}"));
            v.push(x.to_string());
        }
    }
    for sql in c.init.iter().flatten() {
        v.push("--init".into());
        v.push(sql.clone());
    }
    for (on, flag) in [
        (c.sealed, "--sealed"),
        (c.unsigned, "--unsigned"),
        (c.create, "--create"),
        (c.log, "--log"),
    ] {
        if on == Some(true) {
            v.push(flag.into());
        }
    }
    Ok(v)
}

/// `harbor start <name|db.duckdb>` — spawn a detached berth and wait for it.
///
/// **A bare word is a configured berth, never a path.** Reading a bare word as
/// a path is how `medlabs`, typed from the wrong directory, once meant the file
/// ./medlabs — created empty under --create, then served as an empty impostor
/// under the name clients trusted. Resolving it as a name closes that class: it
/// either matches something configured or it is an error, and it can never
/// quietly become a file that is not there.
fn start(rest: Vec<String>) -> Result<(), String> {
    let Some(first) = rest.first() else {
        return Err("which berth? (try: harbor show)".into());
    };
    if first.starts_with('-') || harbor_common::looks_like_path(first) {
        return spawn_detached(rest);
    }
    let name = harbor_common::normalize(first)?;
    let cfg = load_config()?;
    let entry =
        cfg.get(&name).filter(|c| c.is_berth()).ok_or_else(|| unknown_berth(&cfg, &name))?;
    let db = entry.database().expect("is_berth implies a path");
    if !db.exists() && entry.create != Some(true) {
        return Err(format!(
            "{name} is configured, but its database is not there\n          \
             config    {}\n          database  {}  (missing)\n        \
             Fix the path in [connection.{name}], or set create = true there",
            harbor_common::paths::shorten(&harbor_common::config_file()?),
            harbor_common::paths::shorten(&db)
        ));
    }
    // Already serving what was asked for? Then the answer is yes, not an
    // error. `start` names an end state, and a second `start` in a shell you
    // forgot you had should read the same as the first.
    let home = harbor_common::runtime_dir()?;
    if reconcile(&cfg, &home).iter().any(|r| {
        r.name == name && r.state == harbor_common::State::Running
    }) && rest.len() == 1
    {
        return show(Vec::new());
    }
    // Config first, then whatever was typed: parse_opts keeps the last
    // assignment, so an explicit flag wins over the file.
    let mut args = vec![db.display().to_string(), "--name".to_string(), name];
    args.extend(entry_args(entry, &cfg.defaults)?);
    args.extend(rest[1..].iter().cloned());
    spawn_detached(args)
}

/// How a berth's lock file reads — the cheapest liveness answer there is.
///
/// `berth_dead` collapses this to a bool, which is right for `stop`/`forget`
/// (where "no evidence" must never be read as "safe to signal") but loses the
/// bit `show` needs: a lock file that exists and is unheld is the *normal
/// residue of a clean exit*, and must not be confused with no lock at all.
#[derive(PartialEq, Eq, Clone, Copy)]
enum Claim {
    /// No lock file. Nothing has ever claimed this name, or state was swept.
    None,
    /// Someone holds it. The berth is alive — proven, without dialling it.
    Held,
    /// The file is there and nobody holds it. Provably not running.
    Free,
}

fn claim_state(lock: &Path) -> Claim {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let Ok(f) = std::fs::OpenOptions::new().read(true).open(lock) else {
            return Claim::None;
        };
        // Taking it proves nobody else has it; dropping f releases immediately.
        let free = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        match free {
            true => Claim::Free,
            false => Claim::Held,
        }
    }
    #[cfg(windows)]
    {
        match std::fs::OpenOptions::new().read(true).write(true).share_mode(0).open(lock) {
            Ok(_) => Claim::Free,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Claim::None,
            Err(_) => Claim::Held,
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Where a berth actually answers — read from its sidecar, never guessed.
///
/// Guessing is how `show` came to print `<runtime>/<name>.sock` for a berth
/// that was bound to TCP: a plausible path, a wrong answer, and no way for the
/// reader to tell. A berth that has not registered has no address, and says so.
enum Addr {
    /// A unix socket, by absolute path.
    Sock(PathBuf),
    /// host:port, for a berth bound to TCP.
    Tcp(String, u16),
}

impl Addr {
    fn read(j: &serde_json::Value) -> Option<Addr> {
        if let Some(s) = j["socket"].as_str() {
            return Some(Addr::Sock(PathBuf::from(s)));
        }
        let port = u16::try_from(j["port"].as_u64()?).ok()?;
        Some(Addr::Tcp(j["bind"].as_str().unwrap_or("127.0.0.1").to_string(), port))
    }

    /// Copy-pasteable, whole: exactly what another process needs to dial this
    /// berth. Both forms speak the same HTTP, so the TCP form is written as
    /// the URL it is rather than as a bare `host:port` you have to dress up.
    fn full(&self) -> String {
        match self {
            Addr::Sock(p) => harbor_common::paths::shorten(p),
            Addr::Tcp(host, port) => format!("http://{host}:{port}"),
        }
    }
}

/// One berth, from every source that knows anything about it.
struct Row {
    name: String,
    state: harbor_common::State,
    pid: Option<u64>,
    uptime: Option<String>,
    db: String,
    addr: Option<Addr>,
    note: Option<String>,
}

/// Read the runtime directory once, for both sidecars and locks.
fn scan_runtime(
    home: &Path,
) -> (
    std::collections::BTreeMap<String, serde_json::Value>,
    std::collections::BTreeSet<String>,
) {
    let mut sidecars: std::collections::BTreeMap<String, serde_json::Value> = Default::default();
    let mut locks = std::collections::BTreeSet::new();
    let Ok(rd) = std::fs::read_dir(home) else { return (sidecars, locks) };
    for p in rd.filter_map(|e| e.ok().map(|e| e.path())) {
        let Some(stem) = p.file_stem().map(|x| x.to_string_lossy().into_owned()) else { continue };
        match p.extension().and_then(|x| x.to_str()) {
            Some("json") => {
                let v = std::fs::read_to_string(&p)
                    .ok()
                    .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                    .unwrap_or_default();
                sidecars.insert(stem, v);
            }
            Some("lock") => {
                locks.insert(stem);
            }
            _ => {}
        }
    }
    (sidecars, locks)
}

/// Reconcile desired (config) against actual (runtime) into one row per name.
///
/// The flock answers liveness for every row but one, so this makes no network
/// call in the common case: N cheap local opens instead of N round trips, each
/// of which could otherwise cost a 2s read timeout.
///
/// Drift is a string comparison, never a `canonicalize`. A configured database
/// on a disconnected mount must not turn an instant command into a hang —
/// `harbor doctor` is the verb that is allowed to touch the disk.
fn reconcile(cfg: &harbor_common::config::FileConfig, home: &Path) -> Vec<Row> {
    use harbor_common::State;
    let (sidecars, locks) = scan_runtime(home);
    let configured: std::collections::BTreeMap<&str, &harbor_common::config::Connection> =
        cfg.berths().into_iter().collect();

    let mut names: std::collections::BTreeSet<String> = Default::default();
    names.extend(configured.keys().map(|k| k.to_string()));
    names.extend(sidecars.keys().cloned());
    names.extend(locks.iter().cloned());

    let mut rows: Vec<Row> = Vec::new();
    for name in names {
        let side = sidecars.get(&name);
        let conf = configured.get(name.as_str()).copied();
        let claim = claim_state(&home.join(format!("{name}.lock")));

        let live_db = side.and_then(|j| j["db"].as_str()).unwrap_or("").to_string();
        let want_db = conf
            .and_then(|c| c.database())
            .map(|p| p.display().to_string())
            .unwrap_or_default();

        let mut note = None;
        let state = match (claim, side.is_some(), conf.is_some()) {
            (Claim::Held, true, true) if live_db == want_db => State::Running,
            (Claim::Held, true, true) => {
                note = Some(format!(
                    "config now says {} — harbor stop {name} && harbor start {name}",
                    harbor_common::paths::shorten(Path::new(&want_db))
                ));
                State::Drifted
            }
            (Claim::Held, true, false) => {
                note = Some("not in your config — a client summoned it, or it was started by hand".into());
                State::Unmanaged
            }
            // Alive with no registration: mid-boot, or a forget ran under it.
            (Claim::Held, false, _) => {
                note = Some("running but unregistered — starting up, or its sidecar was removed".into());
                State::Unmanaged
            }
            (Claim::Free, true, _) => {
                note = Some(format!("registry says it is running and the lock says otherwise — harbor forget {name}"));
                State::Dead
            }
            // A lock left by a clean exit is normal residue, not a mess.
            (Claim::Free, false, true) | (Claim::None, false, true) => State::Stopped,
            (Claim::Free, false, false) => {
                note = Some(format!("left by a berth that is gone — harbor forget {name}"));
                State::Stale
            }
            // Sidecar, no lock at all: the only row that has to be dialled.
            (Claim::None, true, _) => {
                let alive = match (side.and_then(|j| j["socket"].as_str()), side.and_then(|j| j["port"].as_u64())) {
                    (Some(sock), _) => ready(Path::new(sock)),
                    (None, Some(port)) => ready_tcp("127.0.0.1", port as u16),
                    _ => false,
                };
                match alive {
                    true if conf.is_some() && live_db == want_db => State::Running,
                    true => State::Unmanaged,
                    false => {
                        note = Some(format!("no lock and no answer — harbor forget {name}"));
                        State::Dead
                    }
                }
            }
            (Claim::None, false, false) => continue,
        };

        let uptime = side
            .and_then(|j| j["startedAtMs"].as_u64())
            .and_then(|t| now_ms().checked_sub(t))
            .map(|ms| harbor_common::lifetime::humanize(Duration::from_millis(ms)));

        rows.push(Row {
            state,
            pid: side.and_then(|j| j["pid"].as_u64()).filter(|_| state.is_live()),
            addr: side.and_then(|j| Addr::read(j)).filter(|_| state.is_live()),
            uptime: uptime.filter(|_| state.is_live()),
            db: match (live_db.is_empty(), want_db.is_empty()) {
                (false, _) => harbor_common::paths::shorten(Path::new(&live_db)),
                (true, false) => harbor_common::paths::shorten(Path::new(&want_db)),
                _ => "—".into(),
            },
            note,
            name,
        });
    }
    rows.sort_by(|a, b| (a.state.rank(), &a.name).cmp(&(b.state.rank(), &b.name)));
    rows
}

/// `harbor show [name]` — the fleet, or one berth in detail.
fn show(rest: Vec<String>) -> Result<(), String> {
    use harbor_common::ui::{Cell, Panel, Style, Table};
    if let Some(flag) = rest.iter().find(|a| a.starts_with('-')) {
        return Err(format!("harbor show takes a berth name, not {flag:?}"));
    }
    if rest.len() > 1 {
        return Err("harbor show takes at most one berth name".into());
    }
    let cfg = load_config()?;
    // Read-only: never create or chmod anything just to list.
    let home = harbor_common::runtime_dir()?;
    let rows = reconcile(&cfg, &home);
    let st = Style::stdout().with_choice(cfg.defaults.color.as_deref());

    if let Some(want) = rest.first() {
        let name = harbor_common::normalize(want)?;
        let row = rows
            .iter()
            .find(|r| r.name == name)
            .ok_or_else(|| unknown_berth(&cfg, &name))?;
        let mut p = Panel::new(&row.name)
            .badge(
                match &row.uptime {
                    Some(u) => format!("{} · {u}", row.state.label()),
                    None => row.state.label(),
                },
                row.state.level().into(),
            )
            .field("database", &row.db);
        if let Some(c) = cfg.get(&name) {
            let life = harbor_common::lifetime::resolve(
                Default::default(),
                c.idle_exit.as_deref(),
                cfg.defaults.idle_exit.as_deref(),
                harbor_common::Summoner::Operator,
            )?;
            p = p.field("idle-exit", life.describe());
            p = p.field(
                "config",
                format!(
                    "{}  [connection.{name}]",
                    harbor_common::paths::shorten(&harbor_common::config_file()?)
                ),
            );
        }
        if let Some(a) = &row.addr {
            p = p.field("address", a.full());
        }
        if let Some(n) = &row.note {
            p = p.field_toned("note", n, row.state.level().into());
        }
        print!("{}", p.footer(row.pid.map(|x| format!("pid {x}")).unwrap_or_default()).render(&st));
        return Ok(());
    }

    if rows.is_empty() {
        println!("Nothing configured, nothing running.\n");
        println!("  harbor start <db.duckdb>   remember a database and start it");
        println!("  pilot <db.duckdb>          just open one — a berth starts itself");
        return Ok(());
    }

    let mut t = Table::new(["NAME", "STATE", "PID", "UPTIME", "ADDRESS", "DATABASE"]);
    for r in &rows {
        t.row([
            Cell::new(&r.name),
            Cell::new(r.state.label()).tone(r.state.level().into()),
            Cell::new(r.pid.map(|p| p.to_string()).unwrap_or("—".into())).right(),
            Cell::new(r.uptime.clone().unwrap_or("—".into())).right(),
            Cell::new(r.addr.as_ref().map(Addr::full).unwrap_or("—".into())),
            Cell::new(&r.db),
        ]);
        if let Some(n) = &r.note {
            t.note(r.state.level().into(), n);
        }
    }
    print!("{}", t.render(&st));

    if st.boxed {
        let mut tally: std::collections::BTreeMap<&str, usize> = Default::default();
        for r in &rows {
            *tally.entry(r.state.word()).or_default() += 1;
        }
        let parts: Vec<String> = tally.iter().map(|(w, n)| format!("{n} {w}")).collect();
        println!("\n  {}", parts.join(", "));
        // The free checks only — an entry on a dead mount must not make this hang.
        if let Some((sev, line)) = doctor::summary(&doctor::quick(&cfg)) {
            println!("  {}", st.paint(sev.tone(), &line));
        }
    }
    Ok(())
}

/// `harbor doctor` — the checks nothing else has a moment to make.
fn doctor_cmd(rest: Vec<String>) -> Result<(), String> {
    use harbor_common::ui::{Style, Tone};
    if !rest.is_empty() {
        return Err("harbor doctor takes no arguments".into());
    }
    let cfg = load_config()?;
    let st = Style::stdout().with_choice(cfg.defaults.color.as_deref());
    let findings = doctor::examine(&cfg);
    if findings.is_empty() {
        println!("{}", st.paint(Tone::Green, "✓ nothing to fix"));
        return Ok(());
    }
    for f in &findings {
        println!("{} {}", st.paint(f.severity.tone(), f.severity.glyph()), f.title);
        for d in &f.detail {
            println!("    {}", st.paint(Tone::Dim, d));
        }
        println!("    {}\n", st.paint(Tone::Cyan, &f.fix));
    }
    match doctor::exit_code(&findings) {
        0 => Ok(()),
        _ => Err(format!("{} problem(s) — see above", findings.len())),
    }
}

fn stop_berth(rest: Vec<String>, remove: bool) -> Result<(), String> {
    let name = rest.first().ok_or("which berth? (try: harbor show)")?;
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
                         Use `harbor forget {name}` to clear the registry entry."
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
        // Say what was actually removed. The old wording claimed success for
        // a name that matched nothing at all, which made `forget` a verb that
        // lied about the one thing it does.
        let mut gone: Vec<&str> = Vec::new();
        for f in ["sock", "json", "token"] {
            if std::fs::remove_file(home.join(format!("{name}.{f}"))).is_ok() {
                gone.push(f);
            }
        }
        let _ = std::fs::remove_file(home.join("log").join(format!("{name}.log")));
        // The lock is the last thing to go, and only while we hold it — see
        // unlink_lock_if_free. Nothing else in this file may unlink one.
        if unlink_lock_if_free(&home.join(format!("{name}.lock"))) {
            gone.push("lock");
        }
        match gone.is_empty() {
            true => println!("nothing to forget: {name:?} left no state behind"),
            false => println!(
                "forgot {name:?} — removed its {} (the database file was not touched)",
                gone.join(", ")
            ),
        }
    }
    // What it did, then what the fleet looks like now: the line above names
    // the thing the table cannot show (that it checkpointed, what was
    // removed), and the table answers the question that follows.
    println!();
    show(Vec::new())
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
