//! harbor — a DuckDB database, served.
//!
//! One binary, one grammar, noun first:
//!
//!   harbor                        what's running
//!   harbor <db.duckdb>            open it — REPL, -c "SQL", or stdin; if
//!                                 nothing serves the file, a server is
//!                                 spawned that lives while anyone is connected
//!   harbor <path/to.sock>         connect to a server by its socket
//!   harbor http://host:port       connect to a server over TCP
//!   harbor <db.duckdb> start      start it yourself, until you leave
//!
//! There is no registry and no config: the socket IS the registration, its
//! name is derived from the database's canonical path (socket_for), and the
//! 0700 runtime dir is the local access control. TCP is the one door that
//! needs a credential, so `--port` makes `--token` mandatory.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};


use harbor_common::duration::parse_duration;
use harbor_common::autostart;
use harbor_common::membership::{self, Attached};
use harbor_common::perms::chmod;

mod verbs;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => return harbor::repl::list_main(),
        Some("-h" | "--help" | "help") => {
            print!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Some("-V" | "--version" | "version") => {
            println!("harbor {VERSION}");
            return ExitCode::SUCCESS;
        }
        // A verb with no database in front of it: the noun comes first.
        Some(v) if verbs::Verb::is_verb(v) => {
            eprintln!("harbor: the database comes first — harbor <db.duckdb> {v}");
            return ExitCode::FAILURE;
        }
        _ => {}
    }

    // Noun first, then a bag of bare verbs, then that verb's own flags. A
    // client invocation has no leading verb and falls straight through to
    // cli_main; a management one hands its verb bag to the grammar and carries
    // the resulting plan out right here — two axes, membership then running.
    let db = args.remove(0);
    let split = args.iter().take_while(|a| verbs::Verb::is_verb(a.as_str())).count();
    if split == 0 {
        return harbor::repl::cli_main(std::iter::once(db).chain(args));
    }
    let verb_words: Vec<String> = args.drain(..split).collect();
    let plan = match verbs::plan(&verb_words) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("harbor: {e}");
            return ExitCode::FAILURE;
        }
    };
    let db = PathBuf::from(db);
    let flags = args; // whatever followed the verbs — start's, and only start's

    if plan.run != Some(true) && !flags.is_empty() {
        eprintln!("harbor: only `start` takes options — got: {}", flags.join(" "));
        return ExitCode::FAILURE;
    }

    // Membership first — durable and quick, and it is what a start's lifetime
    // keys off: a listed database is persistent, an unlisted one ephemeral.
    // Detach also tears down any login item, since one for a database you no
    // longer keep makes no sense.
    match plan.attach {
        Some(true) => match membership::attach(&db) {
            Ok((name, Attached::Added)) => eprintln!("harbor: attached {name}"),
            Ok((name, Attached::AlreadyThere)) => eprintln!("harbor: {name} is already attached"),
            Err(e) => {
                eprintln!("harbor: {e}");
                return ExitCode::FAILURE;
            }
        },
        Some(false) => match membership::detach(&db) {
            Ok((name, removed)) => {
                if removed {
                    eprintln!("harbor: detached {name}");
                }
            }
            Err(e) => {
                eprintln!("harbor: {e}");
                return ExitCode::FAILURE;
            }
        },
        None => {}
    }

    // autostart is declarative and defaults off: any lifetime change re-applies
    // it — an explicit `autostart` installs the login item, its absence removes
    // one — and a detach tears it down too. We act before the running axis,
    // since a persistent start blocks at the helm and would defer the install.
    if plan.run.is_some() || plan.attach == Some(false) {
        let name = match membership::name_of(&db) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("harbor: {e}");
                return ExitCode::FAILURE;
            }
        };
        let synced = if plan.autostart {
            autostart::install(&db, &name).inspect(|()| eprintln!("harbor: {name} will start at login"))
        } else {
            autostart::remove(&name).map(|removed| {
                if removed {
                    eprintln!("harbor: {name} will no longer start at login");
                }
            })
        };
        if let Err(e) = synced {
            eprintln!("harbor: {e}");
            return ExitCode::FAILURE;
        }
    }

    // Running. The grammar owns the lifetime — a detached start is ephemeral —
    // so start takes that as a plain fact, not a flag.
    match plan.run {
        Some(true) => match start(db, flags, plan.ephemeral()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("harbor: {e}");
                ExitCode::FAILURE
            }
        },
        Some(false) => match harbor::repl::shutdown(&db) {
            Ok(true) => {
                eprintln!("harbor: {} stopped", db.display());
                ExitCode::SUCCESS
            }
            Ok(false) => {
                eprintln!("harbor: {} was not running", db.display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("harbor: {e}");
                ExitCode::FAILURE
            }
        },
        None => ExitCode::SUCCESS, // a bare attach/detach: membership done
    }
}

const HELP: &str = "\
harbor — a DuckDB database, served

usage:
  harbor                       what's running
  harbor <db.duckdb>           open a database: the REPL on a terminal, or
                               SQL from -c \"...\" / stdin on a pipe. No server
                               behind the file yet? One is spawned for it.
  harbor <path/to.sock>        connect to a server by its unix socket
  harbor http://host:port      connect to a server over TCP
  harbor <db.duckdb> start     start it yourself (foreground): on a terminal
                               you get the prompt and .quit ends the server;
                               headless it runs until SIGTERM
  harbor <db.duckdb> stop      stop the server for this database, if one is
                               running (a quiet no-op if nothing is)
  harbor <db.duckdb> attach    add this database to your list (config.toml) —
                               a listed database is persistent when started
  harbor <db.duckdb> detach    remove it from your list (and its login item)
  harbor <db.duckdb> autostart start it at every login (implies attach + start;
                               `autostart stop` arms login but leaves it off now)
  harbor version               print this binary's version (also -V)

Verbs combine, in any order: `attach start` remembers it and starts it
persistent; `detach start` starts an ephemeral one; `attach` alone just
lists it. At most one of attach/detach and one of start/stop.

The two lifetimes, in one breath — bare: the server is everyone's, it lives
while anyone is connected. start: the server is yours, it lives until you
leave.

client options:
  -c \"SQL\"                     run statements and exit (stdin works too)
  --token <t>                  bearer token for a TCP server (else $HARBOR_TOKEN)
  --mode <m>                   duckbox, duckboxy, markdown, csv, json, jsonlines, line, list, trash
  --json                       shorthand for --mode jsonlines

start options:
  --port <p>           listen on TCP instead of the unix socket (--token
                       becomes mandatory: TCP leaves the 0700 runtime dir,
                       which is what secures the socket)
  --bind <addr>        TCP bind address (with --port; default 127.0.0.1)
  --token <t>          bearer token (TCP only)
  --workers <n>        executor pool size (default 6)
  --memory-limit <s>   DuckDB memory_limit (default 2GB)
  --threads <n>        DuckDB threads (default: DuckDB's own)
  --init <sql>         run SQL at boot, before serving (repeatable) — the door
                       for extensions: --init 'LOAD <ext>'
  --unsigned           allow unsigned extensions (open-time only)
  --sealed             lock the server to SQL on its own database: no host
                       file access, no community extensions
  --statement-timeout <d>  hard deadline ceiling per statement (e.g. 30s)
  --max-temp-size <s>  cap spill-to-disk (e.g. 10GB; default: DuckDB's own)
  --log                log requests to stderr
";

struct Opts {
    db: PathBuf,
    ephemeral: bool,
    port: Option<u16>,
    bind: String,
    token: Option<String>,
    workers: usize,
    memory_limit: String,
    threads: Option<u32>,
    init: Vec<String>,
    log: bool,
    unsigned: bool,
    sealed: bool,
    statement_timeout: Option<Duration>,
    max_temp_size: Option<String>,
}

/// The built-in defaults, before config or flags speak.
fn default_opts(db: PathBuf) -> Opts {
    Opts {
        db,
        ephemeral: false,
        port: None,
        bind: "127.0.0.1".into(),
        token: None,
        workers: harbor::DEFAULT_MAX_INFLIGHT,
        memory_limit: "2GB".into(),
        threads: None,
        init: Vec::new(),
        log: false,
        unsigned: false,
        sealed: false,
        statement_timeout: None,
        max_temp_size: None,
    }
}

/// Fill server options from this database's `[connection.*]` entry, if it has
/// one — the standing settings a bare start should honor. Only a config that
/// loads cleanly is trusted: its `init` runs SQL and `LOAD` can run native
/// code, so an entry from a file anyone else could write is ignored (the same
/// refusal the client applies). The credential is never read here: the token
/// fields belong to the client, and `--token` has no config key — a secret is
/// the operator's explicit word at spawn. `port`/`bind` ARE config keys, but
/// only an explicit start honors them: a summon (`ephemeral`) stays on the
/// unix socket, so opening a database can never silently expose it to the
/// network — and the summoning client is waiting on that socket anyway.
fn apply_berth_config(o: &mut Opts, canon: &Path, ephemeral: bool) {
    use harbor_common::config;
    let cfg = match config::load() {
        Ok(c) => c,
        Err(config::Error::Missing(_)) => return,
        Err(e) => {
            eprintln!("harbor: ignoring config — {e}");
            return;
        }
    };
    // The entry whose database file is the one being started.
    let entry = cfg.berths().into_iter().find_map(|(_, c)| {
        let p = c.database()?;
        (harbor_common::paths::canonical_db(&p).ok()? == *canon).then_some(c)
    });
    let Some(c) = entry else { return };

    if !ephemeral {
        if let Some(p) = c.port {
            o.port = Some(p);
        }
        if let Some(b) = &c.bind {
            o.bind = b.clone();
        }
    }
    if let Some(v) = &c.memory_limit {
        o.memory_limit = v.clone();
    }
    if let Some(v) = c.threads {
        o.threads = Some(v as u32);
    }
    if let Some(v) = c.workers {
        o.workers = v;
    }
    if let Some(v) = &c.max_temp_size {
        o.max_temp_size = Some(v.clone());
    }
    if let Some(v) = &c.statement_timeout
        && let Ok(d) = parse_duration(v)
    {
        o.statement_timeout = Some(d);
    }
    if c.sealed == Some(true) {
        o.sealed = true;
    }
    if c.unsigned == Some(true) {
        o.unsigned = true;
    }
    if c.log == Some(true) {
        o.log = true;
    }
    // The extension/settings door. The entry's `init` runs first — harbor
    // stays agnostic about what it says (INSTALL/LOAD, SET, secrets) — then
    // its `[settings]` block as `SET key = value`, so a setting can tune an
    // extension the init just loaded. Any --init the operator adds on the
    // command line is appended after this (parse_opts), giving it the last
    // word. All of it passes straight to DuckDB at open.
    let mut init = c.init.clone().unwrap_or_default();
    init.extend(c.setting_statements());
    o.init = init;
}

fn parse_opts(mut o: Opts, rest: Vec<String>) -> Result<Opts, String> {
    let mut it = rest.into_iter();
    while let Some(a) = it.next() {
        let mut take = |what: &str| it.next().ok_or(format!("--{what} needs a value"));
        match a.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            "--port" => o.port = Some(take("port")?.parse().map_err(|_| "bad --port")?),
            "--bind" => o.bind = take("bind")?,
            "--token" => o.token = Some(take("token")?),
            "--workers" => o.workers = take("workers")?.parse().map_err(|_| "bad --workers")?,
            "--memory-limit" => o.memory_limit = take("memory-limit")?,
            "--threads" => o.threads = Some(take("threads")?.parse().map_err(|_| "bad --threads")?),
            "--init" => o.init.push(take("init")?),
            "--log" => o.log = true,
            "--unsigned" => o.unsigned = true,
            "--sealed" => o.sealed = true,
            "--statement-timeout" => {
                o.statement_timeout = Some(parse_duration(&take("statement-timeout")?)?)
            }
            "--max-temp-size" => o.max_temp_size = Some(take("max-temp-size")?),
            other => return Err(format!("unexpected argument: {other}")),
        }
    }
    // A typed path is the duckdb-cli contract: open it, existing or not, so a
    // missing file becomes a fresh database rather than an error.
    // The token law, both directions. A unix socket in the 0700 runtime dir
    // is already access-controlled by the filesystem, so a token there is a
    // second lock on a door only you can reach — refused, so nobody believes
    // it does something. TCP is reachable by anything that can dial the
    // address, so there the token is not optional.
    match (&o.port, &o.token) {
        (Some(_), None) => {
            return Err("--port exposes the server beyond this user — --token is mandatory with it".into());
        }
        (Some(_), Some(t)) if t.is_empty() => {
            return Err("--token must not be empty with --port".into());
        }
        (None, Some(_)) => {
            return Err("--token has no meaning on a unix socket — the 0700 runtime dir is the access control (use --port for TCP)".into());
        }
        _ => {}
    }
    #[cfg(windows)]
    if o.port.is_none() {
        return Err("Windows has no unix sockets — start with --port <p> --token <t>".into());
    }
    Ok(o)
}

/// The runtime dir, created and tightened before use — it holds sockets,
/// which are the local access control, so a directory made earlier by hand
/// or under a sloppy umask must not be allowed to stay world-listable.
fn ensure_runtime_dir() -> Result<PathBuf, String> {
    let run = harbor_common::runtime_dir()?;
    harbor_common::perms::ensure_private_dir(&run)?;
    if let Ok(state) = harbor_common::state_root() {
        let _ = chmod(&state, 0o700);
    }
    Ok(run)
}

// ---------------------------------------------------------------------------
// start — the one verb, and the only code path that touches the engine
// ---------------------------------------------------------------------------

fn start(db: PathBuf, rest: Vec<String>, ephemeral: bool) -> Result<(), String> {
    // Ephemerality is the grammar's word (a detached start), or the private
    // signal spawn-on-use sets on the child it launches — never a CLI flag.
    // Either way this server is refcounted: it leaves once nobody's connected.
    // Settled before config is read, because a summon must not inherit the
    // entry's TCP exposure (see apply_berth_config).
    let ephemeral = ephemeral || std::env::var_os("HARBOR_EPHEMERAL").is_some();
    // A database's config entry supplies its standing settings — memory,
    // threads, boot SQL, extensions — so a bare `harbor <db> start` (a summon,
    // an autostart at boot) honors them without flags. Read against the
    // canonical file, so any spelling of the path finds the same entry;
    // explicit flags parsed next override whatever the entry set.
    let canon = harbor_common::paths::canonical_db(&db)?;
    let mut o = default_opts(db);
    apply_berth_config(&mut o, &canon, ephemeral);
    let mut o = parse_opts(o, rest)?;
    o.ephemeral = ephemeral;
    let home = ensure_runtime_dir()?;

    // Where this database answers, derived, never chosen: one file, one
    // socket, every time. A TCP server has no socket to derive — and must
    // not fail on a runtime dir too deep to hold one it will never bind.
    #[cfg(unix)]
    let sock_path = match o.port {
        None => harbor_common::socket_for(&home, &canon)?,
        Some(_) => PathBuf::new(),
    };
    #[cfg(windows)]
    let sock_path = {
        let _ = &home; // created for its 0700 healing; TCP needs no socket
        PathBuf::new()
    };

    // A friendlier answer than the lock error, when the answer is knowable:
    // something already serves this file. Advisory only — the database's own
    // file lock below is the real mutex, so a race here just falls through
    // to that.
    #[cfg(unix)]
    if o.port.is_none() && sock_path.exists() && harbor::repl::sock_ready(&sock_path) {
        return Err(format!(
            "{} is already being served — `harbor {}` connects to it",
            canon.display(),
            o.db.display()
        ));
    }

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

    // The engine — and the mutex. DuckDB locks the database file per
    // process, so of two servers racing for one database exactly one gets
    // past this line; the loser exits here without ever touching the
    // winner's socket. No lock files, no flock protocol: the database
    // guards itself.
    let mut con = duckdb_open(&o)?;
    let duckdb_version: String = con
        .query_strings("SELECT version()")
        .map_err(|e| format!("version: {e}"))?
        .pop()
        .unwrap_or_default();
    // Boot SQL runs on the control connection before the pool forms, so its
    // effects (LOAD, settings, secrets) are instance-wide and in place
    // before the first request. This one flag is the whole extension story —
    // harbor stays agnostic about what an operator loads.
    for sql in &o.init {
        con.execute_batch(sql).map_err(|e| format!("--init {sql:?}: {e}"))?;
    }
    // The ATTACHED CATALOG NAMES, which is what a client must qualify its
    // queries with. Read once, here, because this runs AFTER the boot SQL
    // above — so an ATTACH in --init is included. A later runtime ATTACH is
    // not; /info is a pure in-memory read and must stay one, since it has to
    // keep answering when every worker is busy.
    let databases: Vec<String> = con
        .query_strings(
            "SELECT database_name FROM duckdb_databases() WHERE NOT internal ORDER BY database_name",
        )
        .unwrap_or_default();
    harbor::open_pool(con)?;

    // We hold the database lock, so anything at the socket path is a
    // leftover by definition — a kill -9, a crash — and safe to sweep.
    #[cfg(unix)]
    if o.port.is_none() && sock_path.exists() {
        std::fs::remove_file(&sock_path).map_err(|e| format!("stale socket: {e}"))?;
    }

    #[cfg(unix)]
    let listen = match o.port {
        Some(port) => harbor::Listen::Tcp { bind: o.bind.clone(), port },
        None => harbor::Listen::Unix(sock_path.clone()),
    };
    #[cfg(windows)]
    let listen = harbor::Listen::Tcp { bind: o.bind.clone(), port: o.port.unwrap_or(0) };
    let addr = harbor::start(listen, o.token.clone(), o.workers, o.log)?;
    let tcp = o.port.is_some() || cfg!(windows);
    if !tcp {
        let _ = chmod(&sock_path, 0o600);
    }

    // GET /info: identity, with uptime and the live client count spliced in
    // by the core. This is the whole registry — the list dials it.
    harbor::set_info(serde_json::json!({
        "protocolVersion": 1,
        // The display name clients label this server with — the wire's
        // InfoResponse has always declared it; 0.22.1 and earlier never
        // sent it, and clients showed blank rows for discovered servers.
        "name": canon
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "harbor".into()),
        "harborVersion": VERSION,
        "duckdbVersion": duckdb_version,
        "database": canon.display().to_string(),
        "databases": databases,
        "pid": std::process::id(),
        // The lifetime mode, so a client that restarts this server (to upgrade
        // the binary) can bring it back the same way it was running.
        "ephemeral": o.ephemeral,
    }));

    eprintln!(
        "harbor {VERSION}: serving {} on {} (duckdb {}, memory_limit {})",
        canon.display(),
        addr,
        duckdb_version,
        o.memory_limit
    );

    // Refcounted lifetime: the server lives while anyone is connected.
    // Two constants, not knobs — a startup grace so a spawner that dies
    // before its client connects cannot orphan us, then a short linger at
    // zero so curl bursts and exit/connect races do not flap the server.
    // The env overrides exist for the test suite only; they are not API.
    if o.ephemeral {
        let startup = std::env::var("HARBOR_STARTUP_GRACE_MS")
            .ok().and_then(|v| v.parse().ok())
            .map_or(Duration::from_secs(30), Duration::from_millis);
        let linger = std::env::var("HARBOR_LINGER_MS")
            .ok().and_then(|v| v.parse().ok())
            .map_or(Duration::from_secs(3), Duration::from_millis);
        std::thread::spawn(move || {
            let mut ever_connected = false;
            let mut zero_since: Option<Instant> = None;
            loop {
                std::thread::sleep(Duration::from_millis(200));
                match harbor::connection_count() {
                    // Stopped by someone else; nothing left to decide.
                    None => break,
                    Some(0) => {
                        let since = *zero_since.get_or_insert_with(Instant::now);
                        let allowed = if ever_connected { linger } else { startup };
                        if since.elapsed() >= allowed {
                            eprintln!("harbor: no clients — leaving");
                            let _ = harbor::stop();
                            break;
                        }
                    }
                    Some(_) => {
                        ever_connected = true;
                        zero_since = None;
                    }
                }
            }
        });
    }

    // At the helm: start on a terminal gets the prompt, dialled at this
    // server's own front door. Leaving the prompt ends the server — the
    // start doctrine, enacted: the server is yours, it lives until you leave.
    if !o.ephemeral && std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        let name = canon
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "harbor".into());
        #[cfg(unix)]
        let transport = match o.port {
            Some(p) => harbor::repl::Transport::Tcp(format!("127.0.0.1:{p}")),
            None => harbor::repl::Transport::Unix(sock_path.clone()),
        };
        #[cfg(windows)]
        let transport = harbor::repl::Transport::Tcp(format!("127.0.0.1:{}", o.port.unwrap_or(0)));
        harbor::repl::helm(transport, o.token.clone(), &name);
        let _ = harbor::stop();
    }

    // Blocks until SIGTERM / .quit at the helm / the refcount departure
    // finishes drain + CHECKPOINT.
    let farewell = harbor::wait()?;
    if !tcp {
        let _ = std::fs::remove_file(&sock_path);
    }
    eprintln!("harbor: {} closed ({farewell})", canon.display());
    Ok(())
}

fn duckdb_open(o: &Opts) -> Result<harbor::engine::conn::Conn, String> {
    // The engine loads on first use — the binary itself has no load-time
    // libduckdb dependency, so invocations that never open a database run
    // on machines without the library.
    //
    // These settings can only be chosen when the database is opened, not
    // with a later SET, so they travel as open-time options:
    //   --unsigned  allow_unsigned_extensions — the one door for loading a
    //               locally built, unsigned extension via --init 'LOAD <ext>'.
    //   --sealed    enable_external_access=false + allow_community_extensions
    //               =false — shrinks a token from host access (read_csv of any
    //               file, COPY TO disk, community native code) to a credential
    //               for this one database. For a server an untrusted caller can
    //               reach. Default off: read_csv/COPY are core data workflows
    //               (the test fixtures themselves load CSV), so the safe edge
    //               is the operator's to draw, like TLS.
    // Signed-only, full-access is the default; each is opt-in.
    let mut options: Vec<(&str, &str)> = Vec::new();
    if o.unsigned {
        options.push(("allow_unsigned_extensions", "true"));
    }
    if o.sealed {
        options.push(("enable_external_access", "false"));
        options.push(("allow_community_extensions", "false"));
    }
    let mut con = harbor::engine::conn::open(&o.db, &options)
        .map_err(|e| format!("open {}: {e}", o.db.display()))?;
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
