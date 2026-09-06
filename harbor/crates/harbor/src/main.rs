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
//!   harbor <name> | <footnote>    a listed database, by its name or its
//!                                 number in the list — running or stopped
//!   harbor <db.duckdb> start      bring it up in the background, until you stop it
//!
//! The socket IS the runtime registration: its name is derived from the
//! database's canonical path (`socket_for`). Shared config supplies named
//! connections and standing settings. The 0700 runtime directory protects
//! Unix sockets; TCP, when `--port` adds it, binds IPv4 loopback only. Remote reach
//! and policy belong to an edge proxy.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};


use harbor_common::duration::parse_duration;
use harbor_common::autostart;
use verbs::Running;
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
    // A bare word in front of a verb means a listed database (`harbor medlabs
    // stop`, `harbor 3 start`), dereferenced to the file a running server
    // declares or a config entry names — never a file made from the word
    // (the safety law in looks_like_path).
    let db = if harbor_common::looks_like_path(&db) {
        PathBuf::from(db)
    } else {
        match harbor::repl::deref_db(&db) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("harbor: {e}");
                return ExitCode::FAILURE;
            }
        }
    };
    let flags = args; // whatever followed the verbs — start's, and only start's

    // Only a hand start takes options. The login item runs a bare `start`
    // that reads the database's config.toml entry, so options given to
    // `autostart` would be honored once and silently dropped at every login.
    if plan.autostart == Some(true) && !flags.is_empty() {
        eprintln!(
            "harbor: a login item starts from config.toml, not flags — put {} under [connection.<name>]",
            flags.join(" ")
        );
        return ExitCode::FAILURE;
    }
    if !matches!(plan.run, Some(Running::Start | Running::Restart)) && !flags.is_empty() {
        eprintln!("harbor: only start and restart take options — got: {}", flags.join(" "));
        return ExitCode::FAILURE;
    }

    // Membership first — durable and quick, and it is what a start's lifetime
    // keys off: a listed database is persistent, an unlisted one ephemeral.
    // Detach also removes any login item, since one for a database you no
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
                } else {
                    eprintln!("harbor: {name} was not attached");
                }
            }
            Err(e) => {
                eprintln!("harbor: {e}");
                return ExitCode::FAILURE;
            }
        },
        None => {}
    }

    let name = match membership::name_for(&db) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("harbor: {e}");
            return ExitCode::FAILURE;
        }
    };

    // The login item. Installing it loads it too, so the session manager
    // starts the server now and at every login; the running axis is carried
    // out by the manager, never by a start in this process. Removing it
    // leaves whatever is running alone unless a stop was asked for. A plain
    // start or stop never touches it: stopped stays stopped until the next
    // login, which is what a login item means.
    if let Some(install) = plan.autostart {
        if install {
            let stopped = match plan.run {
                Some(Running::Stop | Running::Restart) => match harbor::repl::shutdown(&db) {
                    Ok(stopped) => stopped,
                    Err(e) => {
                        eprintln!("harbor: {e}");
                        return ExitCode::FAILURE;
                    }
                },
                _ => false,
            };
            if stopped {
                eprintln!("harbor: {} stopped", db.display());
            }
            if plan.run == Some(Running::Stop) {
                return match autostart::arm(&db, &name) {
                    Ok(()) => {
                        eprintln!("harbor: {name} will start at login");
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("harbor: {e}");
                        ExitCode::FAILURE
                    }
                };
            }
            if plan.run == Some(Running::Restart) {
                autostart::unload(&name);
            }
            return match autostart::install(&db, &name, serving(&db)) {
                Ok(autostart::Installed::Started) => match wait_serving(&db, &name) {
                    Ok(sock) => {
                        eprintln!("harbor: {name} serving on {} — it will start at every login", sock.display());
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("harbor: {e}");
                        ExitCode::FAILURE
                    }
                },
                Ok(autostart::Installed::AlreadyRunning) => {
                    eprintln!("harbor: {name} is already running under its login item — `restart` applies a changed config");
                    ExitCode::SUCCESS
                }
                Ok(autostart::Installed::Deferred) => {
                    eprintln!(
                        "harbor: {name} is already being served; it will start at login — `harbor {} restart` hands it over now",
                        db.display()
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("harbor: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        if plan.run == Some(Running::Stop) {
            match harbor::repl::shutdown(&db) {
                Ok(true) => eprintln!("harbor: {} stopped", db.display()),
                Ok(false) => {}
                Err(e) => {
                    eprintln!("harbor: {e}");
                    return ExitCode::FAILURE;
                }
            }
            autostart::unload(&name);
        }
        match autostart::remove(&name) {
            Ok(true) => eprintln!("harbor: {name} will no longer start at login"),
            Ok(false) => eprintln!("harbor: {name} was not set to start at login"),
            Err(e) => {
                eprintln!("harbor: {e}");
                return ExitCode::FAILURE;
            }
        }
        if plan.run != Some(Running::Start) {
            return ExitCode::SUCCESS;
        }
    } else if plan.attach == Some(false) {
        match autostart::remove(&name) {
            Ok(true) => eprintln!("harbor: {name} will no longer start at login"),
            Ok(false) => {}
            Err(e) => {
                eprintln!("harbor: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    // Running. The grammar owns the lifetime — a detached start is ephemeral —
    // so start takes that as a plain fact, not a flag. A restart of a database
    // with a login item is the manager's to do: the server comes back under
    // launchd or systemd with a fresh read of its config, not in this process.
    match plan.run {
        Some(Running::Start) => match start(db, flags, plan.ephemeral()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("harbor: {e}");
                ExitCode::FAILURE
            }
        },
        Some(Running::Stop) => match harbor::repl::shutdown(&db) {
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
        Some(Running::Restart) => {
            if autostart::installed(&name) && !flags.is_empty() {
                eprintln!(
                    "harbor: {name} starts at login from config.toml, not flags — put {} under [connection.{name}]",
                    flags.join(" ")
                );
                return ExitCode::FAILURE;
            }
            match harbor::repl::shutdown(&db) {
                Ok(true) => eprintln!("harbor: {} stopped", db.display()),
                Ok(false) => {}
                Err(e) => {
                    eprintln!("harbor: {e}");
                    return ExitCode::FAILURE;
                }
            }
            if autostart::installed(&name) {
                autostart::unload(&name);
                return match autostart::install(&db, &name, false).and_then(|_| wait_serving(&db, &name)) {
                    Ok(sock) => {
                        eprintln!("harbor: {name} restarted, serving on {} — it will start at every login", sock.display());
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("harbor: {e}");
                        ExitCode::FAILURE
                    }
                };
            }
            match start(db, flags, plan.ephemeral()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("harbor: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        None => ExitCode::SUCCESS, // a bare attach/detach: membership done
    }
}

/// The session manager starts a server asynchronously: block until it
/// answers on its socket, or say why not with the log to read. The same
/// budget a summon gives its child. A start that never comes up is taken
/// back out of the manager, so a database that cannot open is not retried
/// every ten seconds until logout; the item stays registered for the next
/// login and the next `start`.
fn wait_serving(db: &Path, name: &str) -> Result<PathBuf, String> {
    #[cfg(unix)]
    {
        let runtime = harbor_common::runtime_dir()?;
        let canon = harbor_common::paths::canonical_db(db)?;
        let sock = harbor_common::socket_for(&runtime, &canon)?;
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if sock.exists() && harbor::repl::sock_ready(&sock) {
                return Ok(sock);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        autostart::unload(name);
        Err(format!(
            "{} did not come up in 15s — see {}; its login item is unloaded until the next login or `start`",
            canon.display(),
            harbor_common::paths::log_file(&runtime, name).display()
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = name;
        Err(format!("{}: login items need unix sockets", db.display()))
    }
}

/// Is anything answering for this database right now — a hand start, a
/// summon, or the login item's own server?
fn serving(db: &Path) -> bool {
    #[cfg(unix)]
    {
        let Ok(runtime) = harbor_common::runtime_dir() else { return false };
        let Ok(canon) = harbor_common::paths::canonical_db(db) else { return false };
        let Ok(sock) = harbor_common::socket_for(&runtime, &canon) else { return false };
        sock.exists() && harbor::repl::sock_ready(&sock)
    }
    #[cfg(not(unix))]
    {
        let _ = db;
        false
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
  harbor <name> | <footnote>   a database by its name (medlabs) or its number
                               in the list — running, or attached and stopped
  harbor <db.duckdb> start     bring its server up in the background — under
                               the login item when there is one — and return;
                               it runs until `stop`. Headless (no terminal) it
                               runs in place until SIGTERM, which is what a
                               session manager wants
  harbor <db.duckdb> stop      stop the server for this database, if one is
                               running (a quiet no-op if nothing is); a login
                               item brings it back at the next login
  harbor <db.duckdb> restart   stop and start again — under the login item
                               when there is one, re-reading config.toml
  harbor <db.duckdb> attach    add this database to your list (config.toml) —
                               a listed database is persistent when started
  harbor <db.duckdb> detach    remove it from your list (and its login item)
  harbor <db.duckdb> autostart keep it running: starts now under launchd or
                               systemd, at every login, and again after a
                               crash (implies attach; `autostart stop` arms
                               login but leaves it off now)
  harbor <db.duckdb> autostart off
                               drop the login item; a running server is left
                               alone (`autostart off stop` takes both down)
  harbor version               print this binary's version (also -V)

Verbs combine, in any order: `attach start` remembers it and starts it
persistent; `detach start` starts an ephemeral one (it leaves when its last
client does); `attach` alone just
lists it. At most one of attach/detach and one of start/stop/restart.
A login item runs a bare `start`, so its options live in config.toml under
[connection.<name>] — statement-timeout, memory-limit, workers, threads, init.

The two lifetimes, in one breath — bare: the server is everyone's, it lives
while anyone is connected. start: the server is yours, it lives until you
stop it.

client options:
  -c \"SQL\"                     run statements and exit (stdin works too)
  --mode <m>                   duckbox, duckboxy, markdown, csv, json, jsonlines, line, list, trash
  --json                       shorthand for --mode jsonlines

start options:
  --port <p>           also listen on TCP, beside the unix socket — loopback
                       only (127.0.0.1); remote reach and access policy
                       belong to an edge proxy
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
  --foreground         run in this terminal until Ctrl-C, output here, no
                       prompt — for watching a server work
";

struct Opts {
    db: PathBuf,
    ephemeral: bool,
    port: Option<u16>,
    workers: usize,
    memory_limit: String,
    threads: Option<u32>,
    init: Vec<String>,
    log: bool,
    unsigned: bool,
    sealed: bool,
    statement_timeout: Option<Duration>,
    max_temp_size: Option<String>,
    foreground: bool,
}

/// The built-in defaults, before config or flags speak.
fn default_opts(db: PathBuf) -> Opts {
    Opts {
        db,
        ephemeral: false,
        port: None,
        workers: harbor::DEFAULT_MAX_INFLIGHT,
        memory_limit: "2GB".into(),
        threads: None,
        init: Vec::new(),
        log: false,
        unsigned: false,
        sealed: false,
        statement_timeout: None,
        max_temp_size: None,
        foreground: false,
    }
}

/// Fill server options from this database's `[connection.*]` entry, if it has
/// one — the standing settings a bare start should honor. Only a config that
/// loads cleanly is trusted: its `init` runs SQL and `LOAD` can run native
/// code, so an entry from a file anyone else could write is ignored (the same
/// refusal the client applies). `port` IS a config key, but only an explicit
/// start honors it: a summon (`ephemeral`) stays on the unix socket, so
/// opening a database never silently opens its TCP door — and the summoning
/// client is waiting on that socket anyway.
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

    if !ephemeral
        && let Some(p) = c.port
    {
        o.port = Some(p);
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
            "--workers" => o.workers = take("workers")?.parse().map_err(|_| "bad --workers")?,
            "--memory-limit" => o.memory_limit = take("memory-limit")?,
            "--threads" => o.threads = Some(take("threads")?.parse().map_err(|_| "bad --threads")?),
            "--init" => o.init.push(take("init")?),
            "--log" => o.log = true,
            "--foreground" => o.foreground = true,
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
    #[cfg(windows)]
    if o.port.is_none() {
        return Err("Windows has no unix sockets — start with --port <p>".into());
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
    // the login item) honors them without flags. Read against the
    // canonical file, so any spelling of the path finds the same entry;
    // explicit flags parsed next override whatever the entry set.
    let canon = harbor_common::paths::canonical_db(&db)?;
    let mut o = default_opts(db);
    apply_berth_config(&mut o, &canon, ephemeral);
    let typed = rest.clone();
    let mut o = parse_opts(o, rest)?;
    o.ephemeral = ephemeral;
    let home = ensure_runtime_dir()?;

    // Where this database answers, derived, never chosen: one file, one
    // socket, every time. The socket exists whether or not a port does — a
    // port is an additional door, and the socket is what keeps a TCP-exposed
    // server visible to the fleet (the list, DuckTable, join-before-summon).
    #[cfg(unix)]
    let sock_path = harbor_common::socket_for(&home, &canon)?;
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
    if sock_path.exists() && harbor::repl::sock_ready(&sock_path) {
        let name = membership::name_for(&canon)?;
        // At a terminal, asking for a server that is up is asking for the
        // state you have — success, like `systemctl start` on an active unit.
        // Headless it stays a refusal: a manager or a spawn that asked for a
        // server and got none must not read a clean exit as one.
        if !o.foreground && std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            eprintln!("harbor: {name} is already serving on {} — `harbor {name}` connects to it", sock_path.display());
            return Ok(());
        }
        return Err(format!(
            "{} is already being served — `harbor {name}` connects to it",
            canon.display()
        ));
    }

    // At a terminal, `start` brings the server up in the background and
    // returns — under the login item when the database has one, so launchd
    // or systemd owns it from the first second; otherwise as a detached
    // child that runs until `stop`. Only a headless start (a service
    // manager, a spawn, a pipe) or `--foreground` serves from this process.
    #[cfg(unix)]
    if !o.foreground && std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        let name = membership::name_for(&canon)?;
        if !o.ephemeral && autostart::installed(&name) {
            if !typed.is_empty() {
                return Err(format!(
                    "{name} starts at login from config.toml, not flags — put {} under [connection.{name}]",
                    typed.join(" ")
                ));
            }
            // The manager may be unreachable — an ssh session has no gui
            // domain — and then the server still comes up, just not under it.
            match autostart::install(&o.db, &name, false) {
                Ok(_) => {
                    let sock = wait_serving(&o.db, &name)?;
                    eprintln!("harbor: {name} serving on {} under its login item — `harbor {name} stop` ends it", sock.display());
                    return Ok(());
                }
                Err(e) => eprintln!("harbor: {e} — starting it here instead; it will not be under the login item until `restart`"),
            }
        }
        let sock = harbor::repl::start_detached(&o.db, &typed, o.ephemeral)?;
        let lifetime = if o.ephemeral { "it leaves when its last client does" } else { &format!("`harbor {name} stop` ends it") };
        eprintln!("harbor: serving {} on {} — {lifetime}", canon.display(), sock.display());
        return Ok(());
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
    if sock_path.exists() {
        std::fs::remove_file(&sock_path).map_err(|e| format!("stale socket: {e}"))?;
    }

    // The socket is always bound; a port adds the loopback TCP door beside it.
    #[cfg(unix)]
    let listen = match o.port {
        Some(port) => harbor::Listen::Dual { port, sock: sock_path.clone() },
        None => harbor::Listen::Unix(sock_path.clone()),
    };
    #[cfg(windows)]
    let listen = harbor::Listen::Tcp { port: o.port.unwrap_or(0) };
    let addr = harbor::start(listen, o.workers, o.log)?;
    #[cfg(unix)]
    let _ = chmod(&sock_path, 0o600);

    // GET /info: identity, with uptime and the live client count spliced in
    // by the core. This is the whole registry — the list dials it.
    harbor::set_info(serde_json::json!({
        "protocolVersion": 1,
        // The name clients label this server with and the CLI resolves a
        // bare word against: the config key that lists this file when one
        // does, else its stem — the same rule the login item and the stopped
        // row use, so `warehouse` is `warehouse` running or not.
        "name": membership::name_for(&canon).unwrap_or_else(|_| "harbor".into()),
        "harborVersion": VERSION,
        "duckdbVersion": duckdb_version,
        "database": canon.display().to_string(),
        "databases": databases,
        "pid": std::process::id(),
        // The lifetime mode, so a client that restarts this server (to upgrade
        // the binary) can bring it back the same way it was running.
        "ephemeral": o.ephemeral,
        // The TCP door, when one is open (the unix socket needs no
        // advertising — finding it is how a client got here). Always
        // loopback, so the port alone spells the door.
        "port": o.port,
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

    // Blocks until SIGTERM (Ctrl-C in the foreground) or the refcount
    // departure finishes drain + CHECKPOINT.
    let farewell = harbor::wait()?;
    #[cfg(unix)]
    let _ = std::fs::remove_file(&sock_path);
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
    //               =false — shrinks a caller's reach from host access
    //               (read_csv of any file, COPY TO disk, community native
    //               code) to SQL on this one database. For a server an
    //               untrusted caller can reach.
    //               Default off: read_csv/COPY are core data workflows
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
