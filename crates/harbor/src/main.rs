//! harbor — DuckDB wearing a server.
//!
//! One binary, two jobs: `serve` embeds DuckDB and owns one database file;
//! the fleet verbs (`start`, `show`, `add`, `expose`, `stop`, `forget`,
//! `doctor`) manage the berths of ~/.local/state/harbor/runtime from outside,
//! linking no engine code paths at all.
//!
//!   harbor serve  db.duckdb [--name n] [--socket p | --port p] [--token t]
//!   harbor start  <name|db.duckdb>           spawn a detached berth, wait ready
//!   harbor show   [name]                     the fleet, or one berth in detail
//!   harbor add    <db.duckdb> [name]         name a database — a name is a service
//!   harbor expose <name> <port|off>          move it onto TCP, or back off it
//!   harbor stop   <name>                     SIGTERM → drain, CHECKPOINT, hold
//!   harbor forget <name>                     stop + clear registry and entry (never the db)
//!   harbor doctor                            check the config for what nothing else sees
//!   harbor version                           print this binary's version
//!
//! The registry is the filesystem: <name>.sock is the registration,
//! <name>.lock (flock) is the mutex, <name>.json is identity, <name>.token
//! is the credential. No daemon anywhere.

use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod config_edit;
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
        "add" => add_cmd(rest),
        "expose" => expose_cmd(rest),
        "stop" => stop_database(rest, false),
        "forget" => stop_database(rest, true),
        "doctor" => doctor_cmd(rest),
        "-h" | "--help" | "help" => {
            print!("{HELP}");
            Ok(())
        }
        "-V" | "--version" | "version" => {
            println!("harbor {VERSION}");
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
  harbor show   [name]              the fleet, or one database in detail
  harbor start  <name|db.duckdb>    start a database in the background, wait ready
  harbor add    <db.duckdb> [name]  name a database — a name is a service:
                                    it starts on use and runs until you say stop
  harbor expose <name> <port|off>   listen on TCP 127.0.0.1:<port> instead of the
                                    unix socket (off: back to the socket)
  harbor stop   <name>              drain, CHECKPOINT, and hold it stopped
                                    (a held name will not autostart; start lifts the hold)
  harbor forget <name>              stop it and drop it — registry files and its
                                    config entry, never the database
  harbor doctor                     check the config for what nothing else sees
  harbor serve  <db.duckdb> [opts]  own a database, serve it (foreground)
  harbor version                    print this binary's version (also -V)

Bare `harbor` is `harbor show`.

A bare word always names a configured database, never a file — which is what
stops `harbor start medlabs`, run from the wrong directory, from meaning the
file ./medlabs. A path carries a slash or a dot; a name never does.

serve/start options (a config entry may set any of these; a flag here wins):
  --create            allow a database file that does not exist yet (the
                      positional is a PATH; without this flag a missing
                      file is an error, never a fresh database)
  --name <n>          the name to serve under (default: db file stem)
  --socket <path>     unix socket (Unix only; default there: $HARBOR_HOME/runtime/<name>.sock)
  --port <p>          listen on TCP 127.0.0.1:<p> instead of a unix socket
  --bind <addr>       TCP bind address (with --port; default 127.0.0.1)
  --token <t>         bearer token ('' disables auth; default: <name>.token,
                      minted on first serve)
  --workers <n>       executor pool size (default 6)
  --memory-limit <s>  DuckDB memory_limit (default 2GB — fleet-safe)
  --threads <n>       DuckDB threads (default: DuckDB's own)
  --idle-exit <d>     drain, CHECKPOINT and exit after <d> (e.g. 90s, 10m) with
                      no requests and no live sessions (a temp database)
  --init <sql>        run SQL at boot, before serving (repeatable) — the door
                      for extensions: --init 'LOAD <ext>'
  --unsigned          allow unsigned extensions (open-time only; needed to
                      LOAD a locally built, unsigned extension)
  --sealed            lock the server to SQL on its own database: no host file
                      access (read_csv/COPY), no community extensions. For a
                      database an untrusted caller can reach
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
            // Asking any verb for help deserves the help, not "unexpected
            // argument" — serve's options are documented in the one page.
            "-h" | "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
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
            "database file not found: {} (the argument is a path, not a configured name; pass --create to make a new database here)",
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
fn ensure_runtime_dir() -> Result<PathBuf, String> {
    let run = harbor_common::runtime_dir()?;
    harbor_common::perms::ensure_private_dir(&run)?;
    if let Ok(state) = harbor_common::state_root() {
        let _ = chmod(&state, 0o700);
    }
    if let Ok(cfg) = harbor_common::config_root()
        && cfg.exists()
    {
        let _ = chmod(&cfg, 0o700);
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
                    _ => return Err(format!("lock {} keeps changing", path.display())),
                }
            }
            if Instant::now() >= deadline {
                return Err(format!("lock {} is already claimed", path.display()));
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
                    return Err(format!("lock {} is already claimed", path.display()));
                }
            }
        }
    }
}

/// Unlink a lock file, but only while holding it.
///
/// The one law about lock files: *unlink only while holding*. Unlinking a
/// lock another claimant has open lets a third create a fresh inode and
/// flock that: two winners, one database. Holding the lock across the
/// unlink is what makes it safe — a concurrent `serve` either loses the
/// flock and waits, or wins after the unlink and revalidates onto the new
/// inode. `forget` unlinks through here; a departing `serve` unlinks under
/// the flock it already holds. Returns whether anything was removed.
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
    let home = ensure_runtime_dir()?;

    // The flock on <name>.lock is the real mutex: held for process life;
    // if we hold it and the socket file exists, the socket is stale by
    // definition and safe to unlink. Never unlink without the lock.
    let lock_path = harbor_common::lock_file(&home, &o.name);
    // The real reason travels: "already claimed" and "permission denied"
    // send an operator to two different places.
    let lock = claim_lock(&lock_path).map_err(|e| format!("{:?}: {e}", o.name))?;
    std::mem::forget(lock); // hold the flock until the process exits

    let sock_path = o.socket.clone().unwrap_or_else(|| harbor_common::sock_file(&home, &o.name));
    if cfg!(unix) && o.port.is_none() && sock_path.exists() {
        std::fs::remove_file(&sock_path).map_err(|e| format!("stale socket: {e}"))?;
    }

    // Token: an existing <name>.token survives restarts (stable identity for
    // clients and Caddy); minted on first serve. `--token ''` = auth off.
    let token_path = harbor_common::token_file(&home, &o.name);
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

    // The engine. One process, one database: conservative memory default
    // so N berths coexist; printed so nobody is surprised.
    let con = duckdb_open(&o)?;
    let duckdb_version: String = con
        .query_row("SELECT version()", [], |r| r.get(0))
        .map_err(|e| format!("version: {e}"))?;
    // Boot SQL runs on the control connection before the pool forms, so its
    // effects (LOAD, settings, secrets) are instance-wide and in place
    // before the first request. This one flag is the whole extension story —
    // harbor stays agnostic about what an operator loads.
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
        // `show` marks a temp database with its idle window; null = permanent.
        "idleExitMs": o.idle_exit.map(|d| d.as_millis() as u64),
    });
    let json_path = harbor_common::sidecar_file(&home, &o.name);
    let tmp = harbor_common::sidecar_file(&home, &o.name).with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&info).unwrap())
        .and_then(|_| std::fs::rename(&tmp, &json_path))
        .map_err(|e| format!("registry json: {e}"))?;

    eprintln!(
        "harbor {VERSION}: serving {} as {:?} on {} (duckdb {}, memory_limit {})",
        o.db.display(),
        o.name,
        addr,
        duckdb_version,
        o.memory_limit
    );

    // Temp berths: no countable requests AND no live sessions for the
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
                    "harbor: idle {}s with no sessions — exiting",
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
    // Departure: a berth that leaves cleanly leaves the harbor as it found
    // it, so `harbor` shows nothing where nothing runs. Unlink-while-holding
    // is the law (see unlink_lock_if_free), and the flock is still held —
    // claim_lock leaked it for process life — so a concurrent claimant
    // either holds the old inode and revalidates onto the fresh one, or
    // arrives after. The token deliberately stays: it is the berth's stable
    // identity across restarts (clients and Caddy read it).
    let _ = std::fs::remove_file(&lock_path);
    eprintln!("harbor: {:?} closed ({farewell})", o.name);
    Ok(())
}

fn duckdb_open(o: &Opts) -> Result<harbor::duckdb::Connection, String> {
    use harbor::duckdb::{Config, Connection};
    // These four settings can only be chosen when the connection is opened,
    // not with a later SET, so they must reach the Connection here:
    //   --unsigned  allow_unsigned_extensions — the one door for loading a
    //               locally built, unsigned extension via --init 'LOAD <ext>'.
    //   --sealed    enable_external_access=false + allow_community_extensions
    //               =false — shrinks a token from host access (read_csv of any
    //               file, COPY TO disk, community native code) to a credential
    //               for this one database. For a berth an untrusted caller can
    //               reach. Default off: read_csv/COPY are core data workflows
    //               (the test fixtures themselves load CSV), so the safe edge
    //               is the operator's to draw, like TLS.
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
    let home = ensure_runtime_dir()?;
    // start means "desired state: running", whatever spelling asked for it.
    // The hold lifts here — the one place every start funnels through — so a
    // path start is not a back door that leaves the operator's stop
    // half-standing under the very name it just raised.
    if std::fs::remove_file(harbor_common::hold_file(&home, &o.name)).is_ok() {
        println!("{:?} was held — start lifts it", o.name);
    }
    let log_path = harbor_common::log_file(&home, &o.name);
    if let Some(dir) = log_path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("log dir: {e}"))?;
    }
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
            .process_group(0); // spawn, don't fork: detached from our tty/session
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
    let sock = o.socket.clone().unwrap_or_else(|| harbor_common::sock_file(&home, &o.name));
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        // The child dying is an answer, not a timeout: it lost the flock to a
        // berth already serving, or failed outright. Without this check the
        // existing berth answers /ready and this start reports success with the
        // dead child's pid — which an orchestrator then records and signals.
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!(
                "the database did not start ({status}); something may already serve this name — \
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
            let served_by =
                harbor_common::fleet::Sidecar::read(&home, &o.name).and_then(|s| s.pid);
            if served_by == Some(u64::from(child.id())) {
                // The receipt is the fleet. A one-line "ready on <socket>"
                // restated the flags you just typed; the table answers the
                // question you actually had, which is what changed.
                return show_after_change(false);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!("{:?} did not come up in 15s — see {}", o.name, log_path.display()))
}

/// The dial of last resort, for the one row shape a lock file cannot settle.
fn probe(a: &harbor_common::fleet::Addr) -> bool {
    match a {
        #[cfg(unix)]
        harbor_common::fleet::Addr::Sock(p) => ready(p),
        #[cfg(not(unix))]
        harbor_common::fleet::Addr::Sock(_) => false,
        harbor_common::fleet::Addr::Tcp(host, port) => ready_tcp(host, *port),
    }
}

fn ready_tcp(bind: &str, port: u16) -> bool {
    let bind = harbor_common::fleet::dial_host(bind);
    let Ok(mut s) = std::net::TcpStream::connect((bind, port)) else { return false };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    probe_200(&mut s)
}

#[cfg(windows)]
fn registered_tcp(home: &Path, name: &str) -> Option<(String, u16)> {
    let side = harbor_common::fleet::Sidecar::read(home, name)?;
    Some((side.bind.unwrap_or_else(|| "127.0.0.1".into()), side.port?))
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
    let bind = harbor_common::fleet::dial_host(bind);
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
/// Fleet verbs never shrug off a refused or invalid config: treating it as
/// empty would silently reclassify every configured berth as unmanaged and
/// make every stopped one vanish. A confidently wrong table is worse than
/// an error. (Only a MISSING file is the zero-config path.)
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
    let mut msg = format!("no database named {name:?} in your config");
    if let Some(near) = known.iter().find(|k| edit_distance(k, name) <= 2) {
        msg.push_str(&format!("\n        did you mean: {near}?"));
    }
    match known.is_empty() {
        true => msg.push_str("\n        nothing is configured yet — harbor add <db.duckdb>"),
        false => msg.push_str(&format!("\n        configured: {}", known.join(", "))),
    }
    msg.push_str("\n        (a bare word always names a configured database; a path carries a / or a dot)");
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
    // A name is a service: absent an entry idle-exit it is persistent, and
    // no fleet-wide temp window has any say here.
    let life = harbor_common::lifetime::resolve(
        c.idle_exit.as_deref(),
        None,
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
        return Err("which database? (try: harbor show)".into());
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
    // start means "desired state: running", which by definition lifts the
    // operator's hold — the one word that outranks a client's autostart.
    // Said out loud: a durable state change deserves a receipt.
    if std::fs::remove_file(harbor_common::hold_file(&home, &name)).is_ok() {
        println!("{name:?} was held — start lifts it");
    }
    if harbor_common::fleet::reconcile(&cfg, &home, &probe)
        .iter()
        .any(|r| r.name == name && r.state == harbor_common::State::Running)
    {
        if rest.len() == 1 {
            return show_after_change(false);
        }
        // start with flags names a different end state than the one running.
        // Drain first — spawning against the live flock is a race the child
        // always loses, reported as a failure the operator didn't cause.
        drain_live(&home, &name)?;
    }
    // Config first, then whatever was typed: parse_opts keeps the last
    // assignment, so an explicit flag wins over the file.
    let mut args = vec![db.display().to_string(), "--name".to_string(), name];
    args.extend(entry_args(entry, &cfg.defaults)?);
    args.extend(rest[1..].iter().cloned());
    spawn_detached(args)
}



/// The fleet, drawn after a verb that changed it — but only for a human.
///
/// `Style::boxed` is exactly `is_terminal()`, which is the question being
/// asked: someone who typed `harbor start labs` wants to see what changed,
/// and a build script that ran the same line wanted a berth, not a TSV dump
/// in the middle of its log. Quiet on success is the older and better
/// contract for the scripted case; the verbs keep their own one-line
/// messages either way, because "drained and checkpointed" and "removed its
/// sock, json, token" are facts no table carries.
fn show_after_change(lead: bool) -> Result<(), String> {
    if !harbor_common::ui::Style::stdout().boxed {
        return Ok(());
    }
    if lead {
        println!();
    }
    show(Vec::new())
}

/// `harbor show [name]` — the fleet, or one berth in detail.
fn show(rest: Vec<String>) -> Result<(), String> {
    use harbor_common::ui::{Panel, Style};
    if let Some(flag) = rest.iter().find(|a| a.starts_with('-')) {
        return Err(format!("harbor show takes a database name, not {flag:?}"));
    }
    if rest.len() > 1 {
        return Err("harbor show takes at most one database name".into());
    }
    let cfg = load_config()?;
    // Read-only: never create or chmod anything just to list.
    let home = harbor_common::runtime_dir()?;
    let rows = harbor_common::fleet::reconcile(&cfg, &home, &probe);
    let st = Style::stdout().with_choice(cfg.defaults.color.as_deref());

    if let Some(want) = rest.first() {
        if harbor_common::looks_like_path(want) {
            return Err("show takes a database name, not a path — bare harbor lists them".into());
        }
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
                c.idle_exit.as_deref(),
                None,
                harbor_common::Summoner::Operator,
            )?;
            // The running berth outranks the entry: a temp summoned before
            // the name was added really will leave, and the panel must not
            // say "never" while the table says "(temp 90s)".
            let idle = match row.idle_exit_ms {
                Some(ms) => harbor_common::lifetime::humanize(Duration::from_millis(ms)),
                None => life.describe().to_string(),
            };
            p = p.field("idle-exit", idle);
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
        println!("  harbor add <db.duckdb>   name a database — it becomes a service");
        println!("  pilot <db.duckdb>        just open one — served on demand");
        return Ok(());
    }

    print!("{}", harbor_common::fleet::table(&rows).render(&st));

    if st.boxed {
        println!("\n  {}", harbor_common::fleet::tally(&rows));
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
    // The empty case returned above, so this is always the error exit — a
    // health check reads the code, a human reads the count.
    let n = findings.len();
    Err(format!("{n} problem{} — see above", if n == 1 { "" } else { "s" }))
}

/// Drain one live berth — signal, wait out the CHECKPOINT — and say nothing.
/// The single verbs own their receipts; the compound ones (`add`, `expose`)
/// own the silence between their own stop and start.
fn stop_core(home: &Path, name: &str, pid: u64, lock_path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        let _ = (home, lock_path);
        // SIGTERM is the contract: drain, CHECKPOINT, exit (core owns it).
        unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(35);
        while Instant::now() < deadline {
            if unsafe { libc::kill(pid as i32, 0) } != 0 {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // Exiting 0 here would be a lie an operator scripts on. And removing
        // registry files under a live berth is worse: deleting its .lock lets
        // a future serve create a fresh inode, flock it, and claim the same
        // name — two berths, one database, the exact thing the mutex prevents.
        Err(format!(
            "{name:?} (pid {pid}) is still running 35s after SIGTERM — \
             nothing was removed. Escalate by hand if you mean it: kill -9 {pid}"
        ))
    }
    #[cfg(windows)]
    {
        let (bind, port) = registered_tcp(home, name)
            .ok_or_else(|| format!("{name:?} has no registered TCP address"))?;
        let token = std::fs::read_to_string(harbor_common::token_file(home, name))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if !shutdown_tcp(&bind, port, token.as_deref()) {
            return Err(format!("{name:?} (pid {pid}) refused the graceful shutdown request"));
        }
        use harbor_common::fleet::{Claim, claim_state};
        let deadline = Instant::now() + Duration::from_secs(35);
        while Instant::now() < deadline && claim_state(lock_path) != Claim::Free {
            std::thread::sleep(Duration::from_millis(100));
        }
        match claim_state(lock_path) {
            Claim::Free => Ok(()),
            _ => Err(format!(
                "{name:?} (pid {pid}) is still running 35s after shutdown — \
                 nothing was removed. Escalate with Task Manager if you mean it."
            )),
        }
    }
}

/// `harbor add <db.duckdb> [name]` — name a database, making it a service.
///
/// The entry is the promotion: from here the name starts on use and runs
/// until you say stop. The file must already exist — add names data, it does
/// not invent any — and the path lands canonicalized, so the entry means the
/// same file from every working directory.
fn add_cmd(rest: Vec<String>) -> Result<(), String> {
    let db = rest.first().ok_or("add which file? (harbor add <db.duckdb> [name])")?;
    if rest.len() > 2 {
        return Err("add takes a database file and, at most, a name".into());
    }
    if !harbor_common::looks_like_path(db) {
        return Err(format!(
            "{db:?} reads as a name, not a file — add takes the database file \
             (a path carries a / or a dot; a name never does)"
        ));
    }
    let canon = std::fs::canonicalize(harbor_common::paths::expand(db))
        .map_err(|_| format!("no database at {db} — add names data that already exists"))?;
    let name = match rest.get(1) {
        Some(n) => normalize(n)?,
        None => normalize(
            &canon.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
        )?,
    };
    let file = config_edit::add_entry(&name, &canon.display().to_string())?;
    println!(
        "added {name:?} — {}  [connection.{name}] in {}",
        harbor_common::paths::shorten(&canon),
        harbor_common::paths::shorten(&file)
    );
    // If a temp berth is already serving this very file under this name, the
    // promotion finishes on the spot: restart it as the service it just
    // became, so its idle-exit dies with it.
    let home = ensure_runtime_dir()?;
    if let Some(side) = harbor_common::fleet::Sidecar::read(&home, &name)
        && side.idle_exit_ms.is_some()
        && side
            .db
            .as_deref()
            .and_then(|p| std::fs::canonicalize(harbor_common::paths::expand(p)).ok())
            .as_deref()
            == Some(&canon)
        && restart_live(&name)?
    {
        return Ok(());
    }
    println!("(starts on use — or right now: harbor start {name})");
    Ok(())
}

/// `harbor expose <name> [<port>|off]` — where the berth listens.
///
/// Bare `expose <name>` only reports; observation must not mutate. A port
/// moves the berth onto TCP — serve listens on one address, so this is
/// instead of the unix socket, not alongside it — and `off` moves it back.
fn expose_cmd(rest: Vec<String>) -> Result<(), String> {
    let name = rest.first().ok_or("expose which database? (harbor expose <name> <port|off>)")?;
    if harbor_common::looks_like_path(name) {
        return Err("expose takes a database name, not a path — harbor show lists them".into());
    }
    let name = normalize(name)?;
    if rest.len() > 2 {
        return Err("expose takes a name and a port (or off)".into());
    }
    let cfg = load_config()?;
    let entry =
        cfg.get(&name).filter(|c| c.is_berth()).ok_or_else(|| unknown_berth(&cfg, &name))?;
    match rest.get(1).map(String::as_str) {
        None => {
            match entry.port {
                Some(p) => println!(
                    "{name:?} listens on {}:{p}",
                    entry.bind.as_deref().unwrap_or("127.0.0.1")
                ),
                None => println!(
                    "{name:?} listens on its unix socket — \
                     harbor expose {name} <port> moves it to TCP"
                ),
            }
            Ok(())
        }
        Some("off") => {
            // Only what expose wrote comes back out: a hand-written bind is
            // the operator's prose, kept for the next expose.
            config_edit::set_entry_key(&name, "port", None)?;
            println!("{name:?} back on its unix socket");
            reapply(&name)
        }
        Some(p) => {
            let port: u16 =
                p.parse().map_err(|_| format!("{p:?} is not a port (1-65535, or off)"))?;
            if port == 0 {
                return Err("port 0 means \"any\" to the OS — pick a real one".into());
            }
            config_edit::set_entry_key(&name, "port", Some(toml_edit::Value::from(port as i64)))?;
            // Rendering a path must not conjure directories: the pure
            // spelling, not the ensure-and-chmod door.
            let home = harbor_common::runtime_dir()?;
            println!(
                "{name:?} will listen on {}:{port} — token: {}",
                entry.bind.as_deref().unwrap_or("127.0.0.1"),
                harbor_common::paths::shorten(&harbor_common::token_file(&home, &name))
            );
            reapply(&name)
        }
    }
}

/// A config change lands where it matters: on the running berth, by restart.
/// A berth at rest is left at rest — a name starts on use, and the new
/// address boards with it.
fn reapply(name: &str) -> Result<(), String> {
    if !restart_live(name)? {
        println!("(not running — it takes effect when {name:?} next starts)");
    }
    Ok(())
}

/// Drain the berth if — and only if — the flock proves it live. Quiet: the
/// caller owns the receipt. A sidecar alone is a claim, not a heartbeat.
/// Returns whether anything was drained.
fn drain_live(home: &Path, name: &str) -> Result<bool, String> {
    use harbor_common::fleet::{Claim, claim_state};
    let lock_path = harbor_common::lock_file(home, name);
    if claim_state(&lock_path) == Claim::Held
        && let Some(pid) = harbor_common::fleet::Sidecar::read(home, name).and_then(|s| s.pid)
    {
        stop_core(home, name, pid, &lock_path)?;
        return Ok(true);
    }
    Ok(false)
}

/// Drain a proven-live berth and start it again under current config; leave
/// anything else at rest and say so with `false`. `start` prints the receipt.
fn restart_live(name: &str) -> Result<bool, String> {
    let home = ensure_runtime_dir()?;
    if drain_live(&home, name)? {
        start(vec![name.to_string()])?;
        return Ok(true);
    }
    Ok(false)
}

fn stop_database(rest: Vec<String>, remove: bool) -> Result<(), String> {
    let verb = if remove { "forget" } else { "stop" };
    let raw = rest.first().ok_or("which database? (try: harbor show)")?;
    // These verbs act on names. Normalizing a typed path would mint a
    // nonsense name ("./x.duckdb" → "--x-duckdb") and then blame the user
    // for a bare word they never typed.
    if harbor_common::looks_like_path(raw) {
        return Err(format!("{verb} takes a database name, not a path — harbor show lists them"));
    }
    let name = normalize(raw)?;
    let home = ensure_runtime_dir()?;
    let sock = harbor_common::sock_file(&home, &name);
    let lock_path = harbor_common::lock_file(&home, &name);

    // stop is a hold: the name stays down against every client's autostart
    // until `harbor start` says otherwise. Written before the signal because
    // the operator's word holds even while the drain runs long — and only
    // for configured names, since nothing autostarts anything else. Lenient
    // on the config read: a broken config must not make a berth unstoppable.
    let cfg = load_config().ok();
    // forget is a fleet verb, and a remote entry is not a berth: it has no
    // registry files here, and its token-cmd is operator prose no verb may
    // eat. Removing one is an edit made in the file, on purpose.
    if remove
        && let Some(k) = cfg.as_ref().and_then(|c| c.get(&name).map(|e| e.kind()))
        && k != harbor_common::config::Kind::Berth
    {
        return Err(format!(
            "{name:?} is not a berth — its [connection.{name}] entry is yours to remove, in {}",
            harbor_common::paths::shorten(&harbor_common::config_file()?)
        ));
    }
    let held = !remove
        && cfg.as_ref().and_then(|c| c.get(&name).map(|e| e.is_berth())).unwrap_or(false);
    if held {
        let _ = write_private(&harbor_common::hold_file(&home, &name), "");
    }
    if let Some(side) = harbor_common::fleet::Sidecar::read(&home, &name) {
        use harbor_common::fleet::{Claim, claim_state};
        let claim = claim_state(&lock_path);
        if claim == Claim::Held {
            // Someone holds the flock: proven alive, safe to signal — if the
            // sidecar can say whom. A lenient sidecar may not, and stripping
            // or guessing under a live berth is the one thing never done.
            let Some(pid) = side.pid else {
                return Err(format!(
                    "{name:?} is alive (its lock is held) but the sidecar records no pid — \
                     not touching a live berth; find the process, stop it by hand, retry"
                ));
            };
            stop_core(&home, &name, pid, &lock_path)?;
            match held {
                true => println!(
                    "{name:?} stopped (drained, checkpointed, held — harbor start {name} lifts it)"
                ),
                false => println!("{name:?} stopped (drained and checkpointed)"),
            }
        } else {
            // No holder (stale residue), or no lock file at all. Either way
            // the recorded pid is unproven and the OS may have recycled it
            // to a stranger — so nothing is ever signalled from here. With
            // no lock the berth itself gets the last word, by the same probe
            // reconcile uses for exactly this shape.
            if claim == Claim::None && side.addr().as_ref().is_some_and(probe) {
                return Err(format!(
                    "{name:?} still answers but has no lock file — nothing proves pid {} \
                     is this berth, so nothing was signalled or removed; stop the process by hand",
                    side.pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into())
                ));
            }
            if !remove {
                // The asked-for state already holds, so this is success —
                // but the corpse's registry files remain and only forget
                // clears them.
                let pid = side.pid.map(|p| p.to_string()).unwrap_or_default();
                match held {
                    true => println!(
                        "{name:?} is not running (stale pid {pid}) — held now, so it will \
                         not start on use (harbor start {name} lifts it); \
                         harbor forget {name} clears the residue"
                    ),
                    false => println!(
                        "{name:?} is not running (stale pid {pid}) — residue remains; \
                         harbor forget {name} clears it"
                    ),
                }
                return Ok(());
            }
        }
    } else if sock.exists() {
        return Err(format!("{name:?} has a socket but no registry json; kill it by pid"));
    } else if !remove {
        // No runtime state, but the name may still be a configured berth at
        // rest — and the fleet table just showed it. Calling it unknown here
        // reads as a registry inconsistency (observed in the field). A berth
        // already in the asked-for state is success, same outcome-honesty as
        // the still-running error above; only a genuinely unknown name errs.
        let cfg = match cfg {
            Some(c) => c,
            None => load_config()?, // surface the real config error now
        };
        return match cfg.get(&name).filter(|c| c.is_berth()) {
            Some(_) => {
                // "Nothing to stop" would be a lie: the hold above is a
                // durable change, and it is the whole outcome of this call.
                match held {
                    true => println!(
                        "{name:?} was not running — held now, so it will not start \
                         on use (harbor start {name} lifts it)"
                    ),
                    false => println!("{name:?} is not running — nothing to stop"),
                }
                Ok(())
            }
            None => Err(unknown_berth(&cfg, &name)),
        };
    }

    if remove {
        // Say what was actually removed: claiming success for a name that
        // matched nothing at all would make `forget` a verb that lies about
        // the one thing it does.
        let mut gone: Vec<&str> = Vec::new();
        for (f, path) in [
            ("sock", harbor_common::sock_file(&home, &name)),
            ("json", harbor_common::sidecar_file(&home, &name)),
            ("token", harbor_common::token_file(&home, &name)),
            ("hold", harbor_common::hold_file(&home, &name)),
        ] {
            if std::fs::remove_file(path).is_ok() {
                gone.push(f);
            }
        }
        let _ = std::fs::remove_file(harbor_common::log_file(&home, &name));
        // The lock is the last thing to go, and only while we hold it — see
        // unlink_lock_if_free for the law every unlink obeys.
        if unlink_lock_if_free(&harbor_common::lock_file(&home, &name)) {
            gone.push("lock");
        }
        // add's inverse: forget also drops the [connection.<name>] entry, or
        // the name would rise again on next use. A config refusing the edit
        // must not fail the sweep above — say so and finish.
        match config_edit::remove_entry(&name) {
            Ok(true) => gone.push("config entry"),
            Ok(false) => {}
            Err(e) => eprintln!("harbor: {e}"),
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
    show_after_change(true)
}

#[cfg(all(test, unix))]
mod tests {
    use harbor_common::fleet::{Claim, claim_state};
    use std::os::fd::AsRawFd;

    #[test]
    fn the_flock_answers_liveness_three_ways() {
        // A unique temp path so parallel test runs never collide.
        let dir = std::env::temp_dir();
        let path = dir.join(format!("harbor-test-{}.lock", std::process::id()));
        let _ = std::fs::remove_file(&path);

        // No lock file at all → no evidence either way. Nothing may be
        // signalled on this answer, and nothing declared dead.
        assert_eq!(claim_state(&path), Claim::None, "missing lock file proves nothing");

        // A lock file that nobody holds → the berth is provably gone.
        let held = std::fs::File::create(&path).unwrap();
        assert_eq!(claim_state(&path), Claim::Free, "an unheld lock file means the berth is gone");

        // While a process holds the exclusive flock (as a live berth does),
        // the berth is alive — even though it answers nothing here.
        assert_eq!(
            unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0,
            "test should be able to take the lock"
        );
        assert_eq!(claim_state(&path), Claim::Held, "a held lock file means the berth is alive");

        unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_UN) };
        let _ = std::fs::remove_file(&path);
    }
}
