//! pilot — the Harbor client.
//!
//! Zero-config local: a bare name resolves to ~/.local/state/harbor/runtime/<name>.sock —
//! or, for a --port berth, the TCP address its sidecar json registered — plus
//! its token file; config.toml is purely additive (remotes, aliases, taste).
//! TLS is Caddy's job — pilot speaks plain HTTP over UDS/TCP.
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

    // Once; resolve and the render defaults share it.
    let (cfg, cfg_err) = config::load();

    // Bare `pilot` opens what the config says to open, and otherwise shows what
    // there is to open. It deliberately does not pick "the only berth" when
    // there happens to be one — that would make adding a second database
    // silently change what a bare command does.
    let Some(target) = target.or_else(|| cfg.defaults.connection.clone()) else {
        return show_fleet(&cfg);
    };
    let conn = match resolve(&cfg, &target, token, cfg_err.as_ref()) {
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
                return repl::run(&conn, &prompt_name(&target), opts);
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
  pilot                        open [defaults] connection, else show the fleet
  pilot <target>               interactive REPL (highlighting, Tab completion)
  pilot <target> -c \"SQL\"      run one statement
  echo \"SQL\" | pilot <target>  same, from stdin
  pilot version                print this binary's version (also -V)

target:
  name                         a service — starts on use, runs until harbor stop
                               (a stopped-by-hand name stays down until harbor start)
  path/to.duckdb               a session — join its server, or start a temp one
                               (opens or creates the file; exits when idle.
                                a slash or a dot marks a path; a name has neither)
  path/to.sock                 a harbor unix socket
  http://host:port             a harbor TCP listener

options:
  --token <t>                  bearer token (else HARBOR_TOKEN, else <name>.token)
  --mode <m>                   duckbox, duckboxy, markdown, csv, json, jsonlines, line, list, trash
  --json                       shorthand for --mode jsonlines

config: $HARBOR_HOME/config.toml ([defaults] mode/timer/maxrows/nullvalue,
[connection.<name>] url|path + token-file|token-cmd). Remote TLS is Caddy's
job; ssh is the human path to a remote host.
";

/// Resolution order: config.toml name -> live berth name ->
/// plain-HTTP url -> .duckdb path (join-or-spawn) -> socket path. Zero-config
/// local always works; the config is purely additive.
fn resolve(
    cfg: &config::FileConfig,
    target: &str,
    flag_token: Option<String>,
    cfg_err: Option<&harbor_common::config::Error>,
) -> Result<Conn, String> {
    let env_token = std::env::var("HARBOR_TOKEN").ok();

    // One classifier for the whole fleet: a name never contains a dot or a
    // slash, so an argument carrying one is a path — and a url says so
    // outright. The same law harbor consults, from the same crate.
    let spelled_out = target.starts_with("http://")
        || target.starts_with("https://")
        || harbor_common::looks_like_path(target);

    // A bare name is a question only the config can answer, so a config that
    // could not be read must not be answered around. Falling through used to
    // mean: one typo anywhere in the file, the whole file discarded (it is
    // deny_unknown_fields, so a single bad key takes all of it), and then
    // `pilot medlabs` — which the file may well have defined as a remote —
    // silently joins the LOCAL berth that happens to share the name. Same
    // prompt, different data, one warning line scrolled past. A spelled-out
    // target says what it means without the config's help; a name does not.
    if let Some(e) = cfg_err
        && !spelled_out
    {
        return Err(format!(
            "{e}\n        so there is no way to know what {target:?} names. \
             Fix the config, or say what you mean: a path, or http://host:port"
        ));
    }

    // One name law for the whole fleet: harbor normalizes every name it
    // mints, so pilot normalizes every name it looks up — `pilot MedLabs`
    // and `harbor start MedLabs` must land on the same berth.
    let normalized: String;
    let target: &str = if spelled_out {
        target
    } else {
        normalized = harbor_common::normalize(target)?;
        &normalized
    };

    // Explicit config entry shadows a same-named live berth (ssh_config rule).
    if let Some(entry) = cfg.connection.get(target) {
        // Both url and path is a question only the entry's author can
        // answer; picking one silently is how you query the wrong database.
        if entry.url.is_some() && entry.path.is_some() {
            return Err(format!(
                "config entry {target:?} has both url and path — keep the one you mean"
            ));
        }
        let home = config::runtime_dir()?;
        if let Some(side) = harbor_common::fleet::Sidecar::read(&home, target) {
            // Warn only when they actually diverge. When the entry's path IS
            // the database the live berth serves, following the config joins
            // that berth — nothing is shadowed, and warning here read as
            // "pilot is about to force a second load" to more than one user.
            let entry_db = entry.path.as_deref().map(|p| {
                let expanded = config::expand(p);
                std::fs::canonicalize(&expanded).unwrap_or(expanded)
            });
            let live_db = side.db.map(PathBuf::from);
            let same = match (&entry_db, &live_db) {
                (Some(a), Some(b)) => a == b,
                _ => false,
            };
            if !same {
                eprintln!("pilot: config entry {target:?} shadows a running local database of the same name");
            }
        }
        let token = flag_token.or(env_token).or_else(|| config::resolve_token(entry));
        if let Some(url) = &entry.url {
            return Ok(Conn { transport: url_transport(url)?, token });
        }
        if entry.path.is_some() {
            // A name is a service: it starts on use and runs until you say
            // stop. The one word that outranks a client is the operator's —
            // after `harbor stop`, the hold keeps the name down until
            // `harbor start` lifts it. Pilot decides nothing here: it only
            // realizes desired state through harbor's own verb.
            let transport = match berth_transport(&home, target) {
                Some(t) => t,
                None => {
                    if harbor_common::hold_file(&home, target).exists() {
                        return Err(format!(
                            "{target:?} is stopped by hand — harbor start {target} brings it back"
                        ));
                    }
                    exec_harbor_start(&[std::ffi::OsString::from(target)])
                        .map_err(|e| format!("cannot start {target:?}: {e}"))?;
                    berth_transport(&home, target).ok_or_else(|| {
                        format!(
                            "harbor started {target:?} but it never registered — \
                             see harbor show {target}"
                        )
                    })?
                }
            };
            return Ok(Conn { transport, token: token.or_else(|| berth_token(&home, target)) });
        }
        return Err(format!("config entry {target:?} has neither url nor path"));
    }

    if target.starts_with("http://") || target.starts_with("https://") {
        return Ok(Conn { transport: url_transport(target)?, token: flag_token.or(env_token) });
    }
    if harbor_common::looks_like_path(target) {
        let p = config::expand(target);
        // The filesystem says which kind of path this is: a live socket is
        // dialled, and everything else is a database file to open. `.sock`
        // still reads as a socket when the file is not there yet, so a
        // mistyped socket path fails as a socket, loudly, instead of
        // quietly becoming a fresh database.
        if is_socket(&p) || target.ends_with(".sock") {
            #[cfg(unix)]
            return Ok(Conn {
                transport: Transport::Unix(p),
                token: flag_token.or(env_token),
            });
            #[cfg(windows)]
            return Err("Unix socket targets are not supported on Windows; use a database name or http://host:port".into());
        }
        // Summon the owner. A second pilot on the same file joins the same
        // berth instead of "database is locked".
        let life = harbor_common::lifetime::resolve(
            None,
            cfg.defaults.temp_idle_exit.as_deref(),
            harbor_common::Summoner::Client,
        )?;
        let (transport, file_token) = ensure_berth(&p, life)?;
        return Ok(Conn { transport, token: flag_token.or(env_token).or(file_token) });
    }

    let home = config::runtime_dir()?;
    // Zero-config: an unconfigured bare name still joins a live local berth.
    // The sidecar is the registry — socket and --port berths both answer here.
    if let Some(transport) = berth_transport(&home, target) {
        let file_token = berth_token(&home, target);
        return Ok(Conn { transport, token: flag_token.or(env_token).or(file_token) });
    }
    Err(format!("nothing running named {target:?}{}", fleet_hint(&home)))
}

fn berth_token(home: &std::path::Path, name: &str) -> Option<String> {
    let t = std::fs::read_to_string(harbor_common::token_file(home, name)).ok()?;
    let t = t.trim().to_string();
    if t.is_empty() { None } else { Some(t) }
}

/// Where a live berth answers — read from its sidecar, never guessed from a
/// path convention. Guessing is how a --socket berth used to be unreachable
/// by name: it answers where it said it would, not where we'd have put it.
fn berth_transport(home: &std::path::Path, name: &str) -> Option<Transport> {
    match harbor_common::fleet::Sidecar::read(home, name)?.addr()? {
        #[cfg(unix)]
        harbor_common::fleet::Addr::Sock(p) => Some(Transport::Unix(p)),
        #[cfg(not(unix))]
        harbor_common::fleet::Addr::Sock(_) => None,
        harbor_common::fleet::Addr::Tcp(host, port) => Some(Transport::Tcp(format!(
            "{}:{port}",
            harbor_common::fleet::dial_host(&host)
        ))),
    }
}

/// Every berth the registry knows, with how to dial it — the same sidecar
/// jsons `harbor show` reads, so socket and --port berths both appear.
fn berth_names(home: &std::path::Path) -> Vec<String> {
    let (sidecars, _, _) = harbor_common::fleet::scan_runtime(home);
    sidecars.into_keys().collect()
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
            "TLS is Caddy's job, not pilot's: pilot stays TLS-free by design. \
             Use http:// on a trusted network, or ssh to the host and use the socket"
                .into(),
        );
    }
    Err(format!("not a url: {url}"))
}

/// The short name a prompt wears: a berth name stays itself, a path or
/// socket shows its stem, a url drops its scheme. The prompt orients; the
/// full target is one `harbor show` away.
pub fn prompt_name(target: &str) -> String {
    if let Some(rest) = target.split_once("://").map(|(_, r)| r) {
        return rest.trim_end_matches('/').to_string();
    }
    if harbor_common::looks_like_path(target)
        && let Some(stem) = std::path::Path::new(target).file_stem()
    {
        return stem.to_string_lossy().into_owned();
    }
    target.to_string()
}

/// Does this path exist as a unix socket right now?
fn is_socket(p: &std::path::Path) -> bool {
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

/// Join the live berth that owns this file, or exec `harbor` to summon an
/// ephemeral one (idle-exit reaps it). Returns socket + the
/// berth's token, if readable.
fn ensure_berth(
    path: &std::path::Path,
    life: harbor_common::lifetime::Lifetime,
) -> Result<(Transport, Option<String>), String> {
    let home = config::runtime_dir()?;
    // A file being created has no inode to canonicalize yet — resolve its
    // parent instead, so the sidecar still records one absolute truth per
    // file and a later pilot from another cwd joins instead of colliding.
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| {
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => std::path::Path::new("."),
        };
        match std::fs::canonicalize(parent) {
            Ok(p) => p.join(path.file_name().unwrap_or_default()),
            Err(_) => path.to_path_buf(),
        }
    });

    // Already owned? The sidecar json says which berth claims this file.
    let (sidecars, _, _) = harbor_common::fleet::scan_runtime(&home);
    for (name, side) in &sidecars {
        if side.db.as_deref() == Some(&canon.display().to_string())
            && let Some(transport) = berth_transport(&home, name)
        {
            return Ok((transport, berth_token(&home, name)));
        }
    }

    // Summon. Pilot never links DuckDB: the owner is the harbor binary.
    // The name is minted by the same law harbor mints them with.
    let stem = canon.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let name = harbor_common::normalize(&stem)
        .map_err(|_| format!("cannot derive a database name from {}", path.display()))?;
    // The operator's stop outranks a typed path exactly as it outranks a
    // typed name: whichever spelling would raise this berth, the hold
    // refuses it, and only harbor's own verb lifts it.
    if harbor_common::hold_file(&home, &name).exists() {
        return Err(format!(
            "{name:?} is stopped by hand — harbor start {name} brings it back"
        ));
    }
    // The sidecar scan above found no berth owning THIS file — so a live
    // berth under the derived name is serving a DIFFERENT database. Summoning
    // would only collide on the name, and joining it would silently query the
    // wrong data. Name both files and the way out instead.
    if sidecars.contains_key(&name) {
        let other = sidecars
            .get(&name)
            .and_then(|s| s.db.clone())
            .unwrap_or_else(|| "another database".to_string());
        return Err(format!(
            "{name:?} is running but serves {other}, not {} — stop it (`harbor stop {name}`) or serve this file under a different name (`harbor start {} --name <other>`)",
            canon.display(),
            canon.display()
        ));
    }
    let mut args: Vec<std::ffi::OsString> = vec![canon.clone().into()];
    args.push("--name".into());
    args.push(name.clone().into());
    // A typed path is the duckdb-cli contract: open it, existing or not.
    // A configured NAME over a missing file still blocks — that guard
    // protects names clients trust; a path names only itself.
    if !canon.exists() {
        args.push("--create".into());
    }
    // A Lifetime knows its own argv, so "never" reaches harbor as the absence
    // of --idle-exit rather than as a duration string harbor cannot parse.
    args.extend(life.to_args().into_iter().map(std::ffi::OsString::from));
    exec_harbor_start(&args).map_err(|e| format!("cannot start {}: {e}", canon.display()))?;
    let transport = berth_transport(&home, &name)
        .ok_or_else(|| format!("harbor start returned without registering {name:?}"))?;
    Ok((transport, berth_token(&home, &name)))
}

/// Pilot's only fleet-touching act is connecting, so anything it starts is
/// started through harbor's own verb — one summon path, owned by the binary
/// that owns the rules. No announcement on success: the operator asked for a
/// database and gets a prompt, not a narration.
fn exec_harbor_start(args: &[std::ffi::OsString]) -> Result<(), String> {
    let harbor = std::env::var("HARBOR_BIN").unwrap_or_else(|_| "harbor".to_string());
    let status = std::process::Command::new(&harbor)
        .arg("start")
        .args(args)
        // pilot is about to draw a prompt; harbor's fleet table is not
        // pilot's output. Failures still speak — harbor writes to stderr.
        .stdout(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("cannot run {harbor:?} (is harbor installed?): {e}"))?;
    match status.success() {
        true => Ok(()),
        false => Err("harbor start failed".into()),
    }
}

fn fleet_hint(home: &std::path::Path) -> String {
    let names = berth_names(home);
    if names.is_empty() { String::new() } else { format!("; running: {}", names.join(", ")) }
}

/// The dial reconcile needs for the one row a lock file cannot settle.
/// `/ready` is unauthenticated by design, so this needs no token.
fn probe(a: &harbor_common::fleet::Addr) -> bool {
    let transport = match a {
        #[cfg(unix)]
        harbor_common::fleet::Addr::Sock(p) => Transport::Unix(p.clone()),
        #[cfg(not(unix))]
        harbor_common::fleet::Addr::Sock(_) => return false,
        harbor_common::fleet::Addr::Tcp(host, port) => Transport::Tcp(format!("{host}:{port}")),
    };
    matches!(
        http::request(&transport, &endpoint::READY, None, None, Some(Duration::from_secs(2))),
        Ok(r) if r.status == 200
    )
}

/// Bare `pilot` with no default connection: the same fleet `harbor` draws.
///
/// The same table from the same reconcile, not a second one that agrees most
/// of the time. Stopped berths belong here too — pilot summons one on demand,
/// so "not running" is not "not openable".
fn show_fleet(cfg: &config::FileConfig) -> ExitCode {
    use harbor_common::ui::{Style, Tone};
    let home = match config::runtime_dir() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("pilot: {e}");
            return ExitCode::FAILURE;
        }
    };
    let rows = harbor_common::fleet::reconcile(cfg, &home, &probe);
    let st = Style::stdout().with_choice(cfg.defaults.color.as_deref());
    if rows.is_empty() {
        println!("Nothing configured, nothing running.\n");
        println!(
            "  {} {}",
            st.paint(Tone::Green, "pilot <db.duckdb>"),
            st.paint(Tone::Dim, "open a database file — served on demand")
        );
        return ExitCode::SUCCESS;
    }
    print!("{}", harbor_common::fleet::table(&rows).render(&st));
    if st.boxed {
        // One line, not two: the count and the invitation are the same
        // thought — here is the fleet, here is how you open one of it — and
        // the arrow carries that so the eye does not have to travel down to
        // find out there was more. Dim arrow, lit command: the only part
        // worth typing is the only part that is bright.
        println!(
            "\n  {} {} {} {}",
            harbor_common::fleet::tally(&rows),
            st.paint(Tone::Dim, "➜"),
            st.paint(Tone::Green, "pilot <name>"),
            st.paint(Tone::Dim, "to open one")
        );
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
                    eprintln!("\npilot: second interrupt — leaving (the server keeps cancelling)");
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
