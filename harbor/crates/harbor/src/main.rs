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
//!   harbor <db.duckdb> backup     its contents, as files you can read
//!   harbor <new.duckdb> restore <dir>  a new database from those files
//!
//! The socket IS the runtime registration: its name is derived from the
//! database's canonical path (`socket_for`). Shared config supplies named
//! connections and standing settings. The 0700 runtime directory protects
//! Unix sockets; TCP, when `--port` adds it, binds IPv4 loopback only. Remote
//! reach and policy belong to an edge proxy.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use harbor_common::autostart;
use harbor_common::duration::parse_duration;
use harbor_common::membership::{self, Attached};
use harbor_common::perms::chmod;
use verbs::{Plan, Running};

mod backup;
mod update;
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
        Some("update") => return update::main(&args[1..]),
        // A verb with no database in front of it: the noun comes first. The
        // attached names are the short way to say it, so name them.
        Some(v) if verbs::Verb::is_verb(v) => {
            eprintln!("harbor: the database comes first — harbor <db.duckdb|name|footnote> {v}");
            let names = attached_names();
            if let Some(first) = names.first() {
                eprintln!("harbor: attached: {} — harbor {first} {v}", names.join(", "));
            }
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
        // One usage text: `harbor <db> -h` asks the same question `harbor -h` does.
        if wants_help(&args) {
            print!("{HELP}");
            return ExitCode::SUCCESS;
        }
        return harbor::repl::cli_main(std::iter::once(db).chain(args));
    }
    let verb_words: Vec<String> = args.drain(..split).collect();
    let flags = args; // whatever followed the verbs — the one verb's own options
    match dispatch(&db, &verb_words, flags) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("harbor: {e}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(db: &str, verb_words: &[String], flags: Vec<String>) -> Result<(), String> {
    // A URL names a server, wherever it runs, and no file this machine can
    // act on. Stopping it is the one verb that needs nothing but the server.
    // It is read as the client reads one: `HTTP://[::1]` is `[::1]:9495`.
    if let Some(addr) = harbor::repl::url_server(db) {
        return match (verb_words, flags.is_empty()) {
            ([stop], true) if stop == "stop" => stop_url(db, &addr?),
            _ => Err(format!(
                "{db} names a server, not a database file: `harbor {db} stop` is the verb it takes — \
                 the rest need the file, on the machine that holds it"
            )),
        };
    }

    // The one-shots come off first. They act on the database's CONTENTS, not
    // its lifetime, so they combine with nothing and carry their own flags —
    // neither of which the plan grammar has anywhere to put.
    if let Some(v) = verbs::Verb::parse(&verb_words[0]).filter(|v| v.is_oneshot()) {
        if verb_words.len() > 1 {
            return Err(format!("{} runs alone — drop {}", verb_words[0], verb_words[1..].join(" ")));
        }
        let db = db_path(db)?;
        return match v {
            verbs::Verb::Backup => backup::backup(&db, &flags),
            _ => backup::restore(&db, &flags),
        };
    }

    let plan = verbs::plan(verb_words)?;
    let db = db_path(db)?;
    // Only a hand start takes options. The login item runs a bare `start`
    // that reads the database's config.toml entry, so options given to
    // `autostart` would be honored once and silently dropped at every login.
    if plan.autostart == Some(true) && !flags.is_empty() {
        return Err(format!(
            "a login item starts from config.toml, not flags — put {} under [connection.<name>]",
            flags.join(" ")
        ));
    }
    if !matches!(plan.run, Some(Running::Start | Running::Restart)) && !flags.is_empty() {
        return Err(format!("only start and restart take options — got: {}", flags.join(" ")));
    }
    enact(&db, &plan, flags)
}

/// Whether a client invocation asks for help: `-h` anywhere but as the value
/// of an option that takes one.
fn wants_help(args: &[String]) -> bool {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => return true,
            "-c" | "--command" | "--mode" | "--block-size" => {
                it.next();
            }
            _ => {}
        }
    }
    false
}

/// Carry out a validated plan: membership first — durable and quick, and
/// what a start's lifetime keys off — then the login item, then the running
/// axis.
fn enact(db: &Path, plan: &Plan, flags: Vec<String>) -> Result<(), String> {
    // A login item under a name another database's item holds is refused
    // before the attach `autostart` implies, which would otherwise stay.
    if plan.autostart == Some(true) {
        autostart::claimable(db, &membership::name_for(db)?)?;
    }
    let filed = match plan.attach {
        Some(true) => {
            let (name, how) = membership::attach(db)?;
            match how {
                Attached::Added => eprintln!("harbor: attached {name}"),
                Attached::AlreadyThere => eprintln!("harbor: {name} is already attached"),
            }
            Some(name)
        }
        Some(false) => {
            let (name, removed) = membership::detach(db)?;
            if removed {
                eprintln!("harbor: detached {name}");
            } else {
                eprintln!("harbor: {name} was not attached");
            }
            Some(name)
        }
        None => None,
    };
    // The name a login item is filed under: the key the database was just
    // attached or detached under, else the key that lists it, else its stem.
    // A name can be another file's, so every step below that touches an item
    // first asks whether the item is this database's (`keeps`).
    let name = match filed {
        Some(name) => name,
        None => membership::name_for(db)?,
    };

    // The login item. Installing it loads it too, so the session manager
    // starts the server now and at every login; the running axis is carried
    // out by the manager, never by a start in this process. Removing it
    // leaves whatever is running alone unless a stop was asked for. A plain
    // start or stop never touches it: stopped stays stopped until the next
    // login, which is what a login item means. Detach removes the item too,
    // since one for a database you no longer keep makes no sense.
    match plan.autostart {
        Some(true) => return autostart_on(db, &name, plan.run),
        Some(false) => {
            if plan.run == Some(Running::Stop) {
                stop(db)?;
                if autostart::keeps(db, &name) {
                    autostart::unload(&name);
                }
            }
            disarm(db, &name, true)?;
            if !matches!(plan.run, Some(Running::Start | Running::Restart)) {
                return Ok(());
            }
        }
        None if plan.attach == Some(false) => disarm(db, &name, false)?,
        None => {}
    }

    // Running. The grammar owns the lifetime — a detached start is ephemeral —
    // so start takes that as a plain fact, not a flag.
    match plan.run {
        Some(Running::Start) => start(db.to_path_buf(), flags, plan.ephemeral(), at_terminal()),
        Some(Running::Stop) => {
            if !stop(db)? {
                eprintln!("harbor: {} was not running", db.display());
            }
            Ok(())
        }
        Some(Running::Restart) => restart(db, &name, flags, plan),
        None => Ok(()), // a bare attach/detach: membership done
    }
}

/// `autostart`, with whatever running verb came beside it: the item is
/// written and, unless a stop was asked for, loaded, so the manager starts
/// the server now.
fn autostart_on(db: &Path, name: &str, run: Option<Running>) -> Result<(), String> {
    if matches!(run, Some(Running::Stop | Running::Restart)) {
        stop(db)?;
    }
    autostart::arm(db, name)?;
    if run == Some(Running::Stop) {
        eprintln!("harbor: {name} will start at login");
        return Ok(());
    }
    if run == Some(Running::Restart) {
        autostart::unload(name);
    }
    match autostart::install(db, name, serving(db))? {
        autostart::Installed::Started => {
            let sock = wait_serving(db, name)?;
            eprintln!("harbor: {name} serving on {} — it will start at every login", sock.display());
        }
        autostart::Installed::AlreadyRunning => {
            eprintln!("harbor: {name} is already running under its login item — `restart` applies a changed config");
        }
        autostart::Installed::Deferred => eprintln!(
            "harbor: {name} is already being served; it will start at login — `harbor {} restart` hands it over now",
            db.display()
        ),
    }
    Ok(())
}

/// Remove this database's login item. An item filed under the name that runs
/// some other file is left alone, and said so when `asked`.
fn disarm(db: &Path, name: &str, asked: bool) -> Result<(), String> {
    if autostart::keeps(db, name) {
        autostart::remove(name)?;
        eprintln!("harbor: {name} will no longer start at login");
    } else if asked && autostart::installed(name) {
        eprintln!("harbor: the login item named {name} runs another database — left alone");
    } else if asked {
        eprintln!("harbor: {name} was not set to start at login");
    }
    Ok(())
}

/// Stop the server for this database, if one is up, and say so.
fn stop(db: &Path) -> Result<bool, String> {
    let stopped = harbor::repl::shutdown(db)?;
    if stopped {
        eprintln!("harbor: {} stopped", db.display());
    }
    Ok(stopped)
}

/// Stop and start again, as it was. A database with a login item comes back
/// under it, the manager re-reading config.toml. Any other comes back in the
/// background with the options the running server was started with, from
/// the directory it was started in, and with the same lifetime, unless
/// options are typed here or `attach`/`detach` says otherwise. Everything
/// the start reads is read first: a restart that cannot start stops nothing.
fn restart(db: &Path, name: &str, flags: Vec<String>, plan: &Plan) -> Result<(), String> {
    let back = comeback(db, name, flags, plan.attach)?;
    stop(db)?;
    if back.item {
        autostart::unload(name);
        autostart::install(db, name, false)?;
        let sock = wait_serving(db, name)?;
        eprintln!("harbor: {name} restarted, serving on {} — it will start at every login", sock.display());
        return Ok(());
    }
    if let Some(dir) = &back.cwd
        && let Err(e) = std::env::set_current_dir(dir)
    {
        eprintln!("harbor: it was started in {}, which is gone ({e}) — starting it from here", dir.display());
    }
    start(back.canon, back.flags, back.ephemeral, true)
}

/// What a restart brings back, settled while the server still runs.
pub(crate) struct Comeback {
    canon: PathBuf,
    /// Under its login item, which reads config.toml itself.
    item: bool,
    ephemeral: bool,
    flags: Vec<String>,
    /// The directory it was started in, so a relative path in its options
    /// names the same file.
    cwd: Option<PathBuf>,
}

/// Settle a restart before anything stops: the options, and a config and
/// options that a start takes. A refusal leaves the server as it is, and
/// says so. A server started by a harbor that records no options cannot be
/// brought back as it was, so it is refused unless the options are typed:
/// guessing would bring a `--sealed` server back open, or a `--port` one
/// without its door.
pub(crate) fn comeback(db: &Path, name: &str, flags: Vec<String>, attach: Option<bool>) -> Result<Comeback, String> {
    let canon = harbor_common::paths::canonical_db(db)?;
    let item = autostart::keeps(db, name);
    let was = running_info(db);
    let refuse = |e: String| match &was {
        Some(_) => format!("not restarting {name}: {e} — it was not stopped"),
        None => format!("not starting {name}: {e}"),
    };
    if item && !flags.is_empty() {
        return Err(refuse(format!(
            "it starts at login from config.toml, not flags — put {} under [connection.{name}]",
            flags.join(" ")
        )));
    }
    let ephemeral = match attach {
        Some(attached) => !attached,
        None => was.as_ref().is_some_and(|i| i["ephemeral"] == true),
    };
    let (flags, cwd) = match &was {
        Some(info) if !item && flags.is_empty() => started_with(db, info).ok_or_else(|| {
            let port = info["port"].as_u64().map_or(String::new(), |p| format!(" --port {p}"));
            let shown = harbor_common::paths::shorten(&canon);
            refuse(format!(
                "it runs harbor {}, which keeps no record of the options it was started with; give them \
                 (`harbor {shown} restart{port} …`), or `harbor {shown} stop` and `start` it",
                info["harborVersion"].as_str().unwrap_or("older than this one")
            ))
        })?,
        _ => (flags, None),
    };
    let mut o = default_opts(canon.clone());
    apply_berth_config(&mut o, &canon, ephemeral && !item).map_err(refuse)?;
    parse_opts(o, flags.clone()).map_err(refuse)?;
    Ok(Comeback { canon, item, ephemeral, flags, cwd })
}

/// Where a server keeps the options it was started with and the directory
/// it was started in: beside its socket, in the runtime directory only its
/// user may enter, readable by its user alone, since an `--init` can hold a
/// secret. Never in `/info`, which answers whoever reaches the TCP door.
/// Written once it serves, removed when it stops, and marked with its pid,
/// so a record a killed server left is not read for the next.
#[cfg(unix)]
fn started_file(sock: &Path) -> PathBuf {
    sock.with_extension("args")
}

/// The options and directory the server answering for `db`, whose `/info`
/// is `info`, recorded at its start; `None` when it recorded none.
/// `--foreground` is how a server ran, not what it is, so it is left out.
fn started_with(db: &Path, info: &serde_json::Value) -> Option<(Vec<String>, Option<PathBuf>)> {
    #[cfg(unix)]
    {
        let (_, _, sock) = harbor_common::paths::socket_of(db).ok()?;
        let record: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(started_file(&sock)).ok()?).ok()?;
        if record["pid"] != info["pid"] {
            return None;
        }
        let args = record["args"].as_array()?.iter().filter_map(|a| a.as_str()).filter(|a| *a != "--foreground");
        Some((args.map(str::to_string).collect(), record["cwd"].as_str().map(PathBuf::from)))
    }
    #[cfg(not(unix))]
    {
        let _ = (db, info);
        None
    }
}

/// A bare word in front of a verb means a LISTED database (`harbor medlabs
/// stop`, `harbor 3 start`), dereferenced to the file a running server
/// declares or a config entry names — never a file made from the word (the
/// safety law in `looks_like_path`).
fn db_path(db: &str) -> Result<PathBuf, String> {
    if harbor_common::looks_like_path(db) {
        Ok(PathBuf::from(db))
    } else {
        harbor::repl::deref_db(db)
    }
}

fn at_terminal() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
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
        let (runtime, canon, sock) = harbor_common::paths::socket_of(db)?;
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
        harbor_common::paths::socket_of(db).is_ok_and(|(_, _, sock)| sock.exists() && harbor::repl::sock_ready(&sock))
    }
    #[cfg(not(unix))]
    {
        let _ = db;
        false
    }
}

/// The `/info` of the server for this database, when one answers.
fn running_info(db: &Path) -> Option<serde_json::Value> {
    #[cfg(unix)]
    {
        let (_, _, sock) = harbor_common::paths::socket_of(db).ok()?;
        info(&sock)
    }
    #[cfg(not(unix))]
    {
        let _ = db;
        None
    }
}

#[cfg(unix)]
fn info(sock: &Path) -> Option<serde_json::Value> {
    let transport = harbor_http::Transport::Unix(sock.to_path_buf());
    let answer = harbor_http::request(&transport, &wire::endpoint::INFO, None, Some(Duration::from_secs(2))).ok()?;
    if answer.status != 200 {
        return None;
    }
    serde_json::from_str(answer.body_string().ok()?.trim()).ok()
}

/// `harbor http://host:port stop`: the server drains, folds its WAL and
/// exits, the same as a stop over its socket. The one clean stop a server
/// without a unix socket has, which on Windows is every server.
fn stop_url(url: &str, addr: &str) -> Result<(), String> {
    let transport = harbor_http::Transport::Tcp(addr.to_string());
    match harbor_http::request(&transport, &wire::endpoint::SHUTDOWN, None, Some(Duration::from_secs(30))) {
        Ok(answer) if answer.status == 202 => {}
        Ok(answer) => {
            let status = answer.status;
            return Err(format!("{url} refused the stop: HTTP {status} {}", answer.body_string().unwrap_or_default().trim()));
        }
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            eprintln!("harbor: nothing is serving on {url}");
            return Ok(());
        }
        Err(e) => return Err(format!("{url} did not answer the stop: {e}")),
    }
    // Stopped means the port is free: the drain and the CHECKPOINT come first.
    let deadline = Instant::now() + Duration::from_secs(60);
    while std::net::TcpStream::connect(addr).is_ok() {
        if Instant::now() > deadline {
            return Err(format!("{url} is still shutting down after 60s"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!("harbor: {url} stopped");
    Ok(())
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
                               session manager wants. On Windows it always
                               runs in place, until Ctrl-C or a stop by URL
  harbor <db.duckdb> stop      stop the server for this database, if one is
                               running (a quiet no-op if nothing is); a login
                               item brings it back at the next login
  harbor http://host:port stop stop the server on that port: drained and
                               checkpointed, as over its socket
  harbor <db.duckdb> restart   stop and start again in the background, as it
                               was: under the login item when there is one,
                               re-reading config.toml; otherwise with the
                               options, directory and lifetime it was started
                               with, unless new options are given. Refuses,
                               stopping nothing, a server that cannot come
                               back: a config that will not load, or options
                               an older harbor kept no record of
  harbor <db.duckdb> attach    add this database to your list (config.toml) —
                               a listed database is persistent when started
  harbor <db.duckdb> detach    remove it from your list (and its login item)
  harbor <db.duckdb> backup [dir] [--format tsv|parquet] [--strict]
                               write its CONTENTS to a directory: schema.sql,
                               load.sql, and one file per table. Tab-separated
                               by default — greppable, diffable, readable by
                               anything, unlike the .duckdb file itself.
                               Defaults to <db>.backups/<stamp>, and never
                               writes into a directory that is there; the
                               directory appears whole or not at all, and only
                               you can read it.
                               Neither format holds every type, so a table the
                               chosen one cannot carry is written in the other
                               and said out loud; --strict refuses instead
  harbor <new.duckdb> restore <dir> [--block-size <s>]
                               build a NEW database from a backup directory.
                               Refuses an existing file, always — moving the
                               restored one into place is a human's job. The
                               file appears whole or not at all. The only
                               moment block size can be chosen
  harbor <db.duckdb> autostart keep it running: starts now under launchd or
                               systemd, at every login, and again after a
                               crash (implies attach; `autostart stop` arms
                               login but leaves it off now)
  harbor <db.duckdb> autostart off
                               drop the login item; a running server is left
                               alone (`autostart off stop` takes both down)
  harbor version               print this binary's version (also -V)
  harbor update [version]      install the newest release over this binary,
                               or the one named (0.42.0): the install
                               one-liner, run from here. Ends by naming the
                               servers still on the old code and the restart
                               each needs; --restart runs those, --check only
                               says what is newest. A copy installed by
                               Homebrew is upgraded by `brew upgrade` and
                               says so

backup and restore stand alone: they act on a database's contents rather than
its lifetime, so they take no other verb. The rest combine, in any order:
`attach start` remembers it and starts it persistent; `detach start` starts
an ephemeral one (it leaves when its last client does); `attach` alone just
lists it. At most one of attach/detach and one of start/stop/restart. A login
item runs a bare `start`, so its options live in config.toml under
[connection.<name>] — statement-timeout, memory-limit, workers, threads, init.
A config.toml that will not load stops a start, and a restart before it
stops anything, with the reason; a server already running is not touched,
and only a missing config means no settings.

The two lifetimes, in one breath — bare: the server is everyone's, it lives
while anyone is connected. start: the server is yours, it lives until you
stop it.

client options:
  -c \"SQL\"                     run statements and exit (stdin works too)
  --mode <m>                   duckbox, duckboxy, markdown, csv, json, jsonlines, line, list, trash
  --json                       shorthand for --mode jsonlines
  --block-size <s>             when this call CREATES the database, its block
                               size: 16k, 32k, 64k, 128k or 256k. Ignored,
                               with a word, if a server is already up — the
                               size is fixed when the file is made

start options:
  --port <p>           also listen on TCP, beside the unix socket — loopback
                       only (127.0.0.1); remote reach and access policy
                       belong to an edge proxy. Required on Windows
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
  --block-size <s>     block size for a database this call CREATES: 16k, 32k,
                       64k, 128k or 256k (default: DuckDB's own 256k). Fixed
                       at creation — ignored, with a warning, for a file that
                       already exists. 64k suits many small tables; 256k suits
                       few large ones. Also a client option, for the database
                       a bare `harbor <db>` summons into being
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
    block_size: Option<u64>,
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
        block_size: None,
        foreground: false,
    }
}

/// The config keys of every attached database, sorted, or nothing when there
/// is no usable config — a hint never turns into an error of its own.
fn attached_names() -> Vec<String> {
    harbor_common::config::load()
        .map(|cfg| cfg.berths().into_iter().map(|(k, _)| k.to_string()).collect())
        .unwrap_or_default()
}

/// Fill server options from this database's `[connection.*]` entry, if it has
/// one — the standing settings a bare start should honor — and return the
/// `[settings]` keys it ignores, for the start to name. A config that is
/// there but will not load stops the start: it may hold `sealed`, a
/// statement ceiling or the boot SQL, and a server that came up without them
/// would look configured while being open. The error names the file, the
/// line and the reason. Only a missing file means no settings. `port` IS a
/// config key, but only an explicit start honors it: a summon (`ephemeral`)
/// stays on the unix socket, so opening a database never silently opens its
/// TCP door — and the summoning client is waiting on that socket anyway.
fn apply_berth_config(o: &mut Opts, canon: &Path, ephemeral: bool) -> Result<Vec<String>, String> {
    use harbor_common::config;
    let cfg = match config::load() {
        Ok(c) => c,
        Err(config::Error::Missing(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };
    // The entry whose database file is the one being started.
    let entry = cfg.berths().into_iter().find(|(_, c)| {
        c.database()
            .and_then(|p| harbor_common::paths::canonical_db(&p).ok())
            .is_some_and(|p| p == *canon)
    });
    let Some((key, c)) = entry else { return Ok(Vec::new()) };
    let bad = |what: &str, e: String| format!("[connection.{key}] {what} in config.toml: {e}");

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
    if let Some(v) = &c.block_size {
        o.block_size = Some(harbor::parse_block_size(v).map_err(|e| bad("block-size", e))?);
    }
    if let Some(v) = &c.statement_timeout {
        o.statement_timeout = Some(parse_duration(v).map_err(|e| bad("statement-timeout", e))?);
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
    Ok(c.rejected_settings().into_iter().map(str::to_string).collect())
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
            "--block-size" => o.block_size = Some(harbor::parse_block_size(&take("block-size")?)?),
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

/// `background` says whether this start may return once a server is up
/// elsewhere — the start verb at a terminal, and every restart — rather than
/// serve from this process, which a session manager, a spawn and
/// `--foreground` want.
fn start(db: PathBuf, rest: Vec<String>, ephemeral: bool, background: bool) -> Result<(), String> {
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
    let ignored = apply_berth_config(&mut o, &canon, ephemeral).map_err(|e| {
        format!("not starting: {e} — fix config.toml and start again; a server already running is not affected")
    })?;
    for key in ignored {
        eprintln!(
            "harbor: [settings] {key} is ignored — it is chosen when the database is \
             opened, so a SET cannot reach it; use the `block-size` key instead"
        );
    }
    let typed = rest.clone();
    let mut o = parse_opts(o, rest)?;
    o.ephemeral = ephemeral;
    let background = background && !o.foreground;
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
        let shown = harbor_common::paths::shorten(&canon);
        // At a terminal, asking for a server that is up is asking for the
        // state you have — success, like `systemctl start` on an active unit.
        // Headless it stays a refusal: a manager or a spawn that asked for a
        // server and got none must not read a clean exit as one.
        if !background {
            return Err(format!("{} is already being served — `harbor {name}` connects to it", canon.display()));
        }
        // Options describe a server this start would have made. The one
        // that is up was made without them, and only a restart remakes it.
        if !typed.is_empty() {
            return Err(format!(
                "{name} is already serving, and options take effect at a start — `harbor {name} restart {}`",
                typed.join(" ")
            ));
        }
        // A start asks for a server that stays. One that leaves with its
        // last client is not that, and saying "already serving" would be
        // believed until it left.
        if info(&sock_path).is_some_and(|i| i["ephemeral"] == true) {
            return Err(format!(
                "{name} is up only while its clients are connected and leaves with the last one — \
                 `harbor {shown} stop` and `start` again to keep one up until you stop it"
            ));
        }
        eprintln!("harbor: {name} is already serving on {} — `harbor {name}` connects to it", sock_path.display());
        return Ok(());
    }

    // In the background, `start` brings the server up and returns — under
    // the login item when the database has one, so launchd or systemd owns
    // it from the first second; otherwise as a detached child that runs
    // until `stop`. Only a headless start (a service manager, a spawn, a
    // pipe) or `--foreground` serves from this process.
    #[cfg(unix)]
    if background {
        let name = membership::name_for(&canon)?;
        if !o.ephemeral && autostart::keeps(&o.db, &name) {
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
        eprintln!("harbor: serving {} on {} — {lifetime}", harbor_common::paths::display_path(&canon), sock.display());
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
    // The record a restart reads (`started_file`). Without it a restart
    // refuses rather than guesses, so a record that cannot be written is
    // said and the server serves on.
    #[cfg(unix)]
    let started = started_file(&sock_path);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let record = serde_json::json!({
            "pid": std::process::id(),
            "args": typed,
            "cwd": std::env::current_dir().ok(),
        });
        let _ = std::fs::remove_file(&started);
        let written = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&started)
            .and_then(|mut f| std::io::Write::write_all(&mut f, record.to_string().as_bytes()));
        if let Err(e) = written {
            eprintln!("harbor: {}: {e} — a restart will need this server's options typed", started.display());
        }
    }

    // GET /info: identity, with uptime and the live client count spliced in
    // by the core. This is the whole registry — the list dials it.
    harbor::set_info(serde_json::json!({
        "protocolVersion": wire::PROTOCOL_VERSION,
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
        // The lifetime, which a restart keeps. The options it was started
        // with are not published: an `--init` can hold a secret, and this
        // answers anyone who reaches the TCP door (`started_file`).
        "ephemeral": o.ephemeral,
        // The TCP door, when one is open (the unix socket needs no
        // advertising — finding it is how a client got here). Always
        // loopback, so the port alone spells the door: the bound one, which
        // for `--port 0` is the system's choice.
        "port": harbor::tcp_port(),
    }));

    eprintln!(
        "harbor {VERSION}: serving {} on {} (duckdb {}, memory_limit {})",
        harbor_common::paths::display_path(&canon),
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
            // Since when nobody has been connected, and how many had been
            // accepted then.
            let mut quiet: Option<(Instant, usize)> = None;
            loop {
                std::thread::sleep(Duration::from_millis(200));
                match harbor::connections() {
                    // Stopped by someone else; nothing left to decide.
                    None => break,
                    // Counted at accept, so a client that came and went
                    // between two looks is a client all the same, and the
                    // linger starts again from it.
                    Some((0, accepted)) => {
                        if quiet.is_none_or(|(_, seen)| seen != accepted) {
                            quiet = Some((Instant::now(), accepted));
                        }
                        let allowed = if accepted > 0 { linger } else { startup };
                        if quiet.is_some_and(|(since, _)| since.elapsed() >= allowed)
                            && harbor::stop_if_idle(accepted)
                        {
                            eprintln!("harbor: no clients — leaving");
                            break;
                        }
                    }
                    Some(_) => quiet = None,
                }
            }
        });
    }

    // Blocks until SIGTERM (Ctrl-C in the foreground) or the refcount
    // departure finishes drain + CHECKPOINT.
    let farewell = harbor::wait()?;
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(&started);
        let _ = std::fs::remove_file(&sock_path);
    }
    eprintln!("harbor: {} closed ({farewell})", harbor_common::paths::display_path(&canon));
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
    //   --block-size  default_block_size — a database file's block size, fixed
    //               when the file is CREATED. Applies to nothing else: an
    //               existing file keeps the size it was born with, and the
    //               check after open says so rather than letting the option
    //               look like it worked.
    let block_size = o.block_size.map(|n| n.to_string());
    let mut options: Vec<(&str, &str)> = Vec::new();
    if let Some(bs) = &block_size {
        options.push(("default_block_size", bs));
    }
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
    // Asked for a block size but opened a file that already had one? DuckDB
    // accepts the option and ignores it, so without this the config reads as
    // a setting and behaves as a wish. Not fatal — the database is fine, it
    // is only not the shape the operator asked for — but never silent.
    if let Some(want) = o.block_size
        && let Ok(rows) = con.query_strings("SELECT block_size::VARCHAR FROM pragma_database_size()")
        && let Some(got) = rows.first().and_then(|r| r.parse::<u64>().ok())
        && got != want
    {
        eprintln!(
            "harbor: {} has {got}-byte blocks, --block-size asked for {want} — block size is \
             fixed when a database is created; EXPORT DATABASE and replay into a new file \
             to change it",
            o.db.display()
        );
    }
    Ok(con)
}

#[cfg(test)]
mod block_size_tests {

    #[test]
    fn accepts_the_five_sizes_duckdb_takes() {
        for (text, want) in [
            ("16k", 16384u64),
            ("32K", 32768),
            ("64kb", 65536),
            ("128KiB", 131072),
            ("256k", 262144),
            ("65536", 65536), // bare bytes
            (" 64k ", 65536), // trimmed
        ] {
            assert_eq!(harbor::parse_block_size(text).unwrap(), want, "{text}");
        }
    }

    #[test]
    fn refuses_everything_else_and_says_why() {
        // Below the floor, above the ceiling, and a non-power-of-two between
        // them — DuckDB rejects all three, so the flag does too, by name.
        for text in ["8k", "512k", "48k", "0"] {
            let e = harbor::parse_block_size(text).expect_err(text);
            assert!(e.contains("power of two from 16k to 256k"), "{text}: {e}");
        }
        // A unit that is not a unit must not be read as bytes.
        let e = harbor::parse_block_size("64X").expect_err("64X");
        assert!(e.contains("k suffix"), "{e}");
    }
}
