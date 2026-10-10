//! autostart — the login item that keeps `harbor <db> start` running for you.
//!
//! The platform's session manager is the supervisor: a launchd LaunchAgent on
//! macOS, a systemd user unit on Linux. Both run the same bare `start`, which
//! takes its standing options from the database's config.toml entry, so the
//! server launched at login is the fully configured one.
//!
//! The item has two independent facts, and the verbs map onto them the way
//! `brew services` and `systemctl` users expect:
//!
//!   registered — the file exists and the manager knows it (arm / remove)
//!   loaded     — the manager holds the job now (install / unload)
//!
//! `install` registers AND loads: the server starts now, under the manager,
//! and again at every login. `arm` registers only. `remove` unregisters and
//! leaves any running server alone; `unload` takes the job out of the current
//! session. The manager restarts the server after a crash and never after a
//! clean exit, so `harbor <db> stop` stays stopped until the next login.
//!
//! An item is filed under a name, and a name is a file's stem or its config
//! key, which another file can share: `~/a/data.duckdb` and `~/b/data.duckdb`,
//! or a scratch `medlabs.duckdb` under a test `HARBOR_HOME` and the real one.
//! So the item is read back for what it runs — the database file and the
//! `HARBOR_HOME` it carries — and an item is this database's only when both
//! match ([`keeps`]). `arm` refuses to take over another's.
//!
//! Shared so the CLI and DuckTable arm and disarm through the same code and
//! read the same truth for a menu checkmark.

#[cfg(any(target_os = "macos", target_os = "linux"))]
use crate::paths;
use std::path::Path;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::path::PathBuf;

/// What `install` found when it went to run the job.
#[derive(Debug, PartialEq, Eq)]
pub enum Installed {
    /// The manager is starting the server under the item now. It is not
    /// listening yet when this returns — the caller waits on the socket.
    Started,
    /// The manager's own server is already up; nothing to do.
    AlreadyRunning,
    /// Something else is serving the database, so the item was registered
    /// but not run — a run would only fail against the file lock and then
    /// retry. `restart` hands the server over.
    Deferred,
}

/// What an item runs, as read back from its file.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Debug, PartialEq, Eq)]
struct Item {
    db: PathBuf,
    home: Option<PathBuf>,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl Item {
    /// Is this the item for the canonical file `canon` under `home`?
    fn is_for(&self, canon: &Path, home: Option<&Path>) -> bool {
        self.under(home) && paths::canonical_db(&self.db).is_ok_and(|c| c == canon)
    }

    /// Does it carry `home`, however either is spelled (`/tmp/h` is
    /// `/private/tmp/h` on macOS)?
    fn under(&self, home: Option<&Path>) -> bool {
        match (self.home.as_deref(), home) {
            (Some(a), Some(b)) => a == b || a.canonicalize().is_ok_and(|a| b.canonicalize().is_ok_and(|b| a == b)),
            (a, b) => a == b,
        }
    }
}

/// Whether the login item called `name` is this database's: it exists, it
/// serves this file, and it carries the `HARBOR_HOME` in force. Anything that
/// stops, restarts or removes an item asks this first.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn keeps(db: &Path, name: &str) -> bool {
    let (Ok(canon), Ok(home)) = (paths::canonical_db(db), paths::harbor_home()) else {
        return false;
    };
    item(name).is_ok_and(|i| i.is_some_and(|i| i.is_for(&canon, home.as_deref())))
}

/// Whether a login item called `name` exists for the `HARBOR_HOME` in force —
/// the menu checkmark's truth. [`keeps`] also asks which file it serves.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn installed(name: &str) -> bool {
    let Ok(home) = paths::harbor_home() else { return false };
    item(name).is_ok_and(|i| i.is_some_and(|i| i.under(home.as_deref())))
}

/// Refuse, before anything is attached, a name whose login item runs some
/// other database: `autostart` would attach this one and then refuse.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn claimable(db: &Path, name: &str) -> Result<(), String> {
    claim(&paths::canonical_db(db)?, name)
}

/// The item filed under `name`, read back; `None` when there is none, and an
/// error when there is a file that does not say what it runs.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn item(name: &str) -> Result<Option<Item>, String> {
    let path = item_path(name)?;
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_item(&text)
            .map(Some)
            .ok_or_else(|| format!("{} does not say which database it runs", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Refuse to act on an item filed under `name` that runs some other file, or
/// the same file under another `HARBOR_HOME`.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn claim(canon: &Path, name: &str) -> Result<(), String> {
    let home = paths::harbor_home()?;
    match item(name)? {
        Some(i) if !i.is_for(canon, home.as_deref()) => {
            let under = match &i.home {
                Some(h) => format!(" with HARBOR_HOME={}", h.display()),
                None => String::new(),
            };
            Err(format!(
                "the login item {} runs {}{under}, not {} — `autostart off` on that database frees the name",
                item_path(name)?.display(),
                i.db.display(),
                canon.display()
            ))
        }
        _ => Ok(()),
    }
}

/// The environment a login item must carry: the variables that move harbor's
/// home. The manager starts the server with a bare environment, so a shell
/// that set one of these would otherwise compute one socket path and the
/// server another, and the two would never meet.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn homes() -> Vec<(String, String)> {
    ["HARBOR_HOME", "XDG_STATE_HOME", "XDG_CONFIG_HOME"]
        .iter()
        .filter_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()).map(|v| (k.to_string(), v)))
        .collect()
}

/// Refuse a path a login item cannot carry. A newline in one would start a
/// new line of a unit file, where a directive could follow it, and no control
/// character is legal in a plist. Such a path is not something to quote
/// around; the homes the item carries are held to the same rule.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn printable(paths: &[&Path]) -> Result<(), String> {
    let homes = homes();
    let carried = paths.iter().map(|p| p.to_string_lossy()).chain(homes.iter().map(|(_, v)| v.into()));
    for text in carried {
        if text.chars().any(char::is_control) {
            return Err(format!("a login item cannot carry a path with a control character in it: {text:?}"));
        }
    }
    Ok(())
}

/// Register the item: write it (only when its contents change) and enable
/// it. Does not load it. Refuses a name another database's item holds.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn arm(db: &Path, name: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let canon = paths::canonical_db(db)?;
    claim(&canon, name)?;
    let log = paths::log_file(&paths::runtime_dir()?, name);
    // Private from the moment it exists: the log directory sits under the
    // runtime directory, whose mode is what keeps other users off the sockets.
    if let Some(dir) = log.parent() {
        crate::perms::create_dir_private(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let path = item_path(name)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    printable(&[exe.as_path(), canon.as_path(), log.as_path()])?;
    let shown = |p: &Path| p.display().to_string();
    let body = item_body(name, &shown(&exe), &shown(&canon), &shown(&log), &homes());
    if std::fs::read_to_string(&path).ok().as_deref() != Some(body.as_str()) {
        std::fs::write(&path, body).map_err(|e| format!("writing {}: {e}", path.display()))?;
    }
    register(name);
    Ok(())
}

/// The item a start or restart runs under: the one there, as it is — a unit
/// the operator tuned by hand stays tuned — or a fresh one when there is none.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn ready_item(db: &Path, name: &str) -> Result<(), String> {
    if !item_path(name)?.exists() {
        return arm(db, name);
    }
    claim(&paths::canonical_db(db)?, name)?;
    register(name);
    Ok(())
}

// ---------------------------------------------------------------------------
// macOS — a ~/Library/LaunchAgents plist, loaded with `launchctl bootstrap`.
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn domain() -> String {
    // SAFETY: getuid has no preconditions and cannot fail.
    format!("gui/{}", unsafe { libc::getuid() })
}

#[cfg(target_os = "macos")]
fn label(name: &str) -> String {
    format!("harbor.{name}")
}

#[cfg(target_os = "macos")]
fn launchctl(args: &[&str]) -> bool {
    std::process::Command::new("launchctl")
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Clear any disable launchd may hold for the label from an earlier life.
#[cfg(target_os = "macos")]
fn register(name: &str) {
    launchctl(&["enable", &format!("{}/{}", domain(), label(name))]);
}

/// Register and run: the server starts now under launchd and at every login.
/// `serving` says whether something already answers for this database. A job
/// launchd still holds with no process — what a clean `stop` leaves, since
/// KeepAlive only revives failures — is booted out and loaded afresh: a
/// fresh load runs at once, where a kickstart of the old job waits out
/// launchd's throttle from its last exit. A job whose process is already up
/// — serving, or still opening the database — is left alone.
#[cfg(target_os = "macos")]
pub fn install(db: &Path, name: &str, serving: bool) -> Result<Installed, String> {
    ready_item(db, name)?;
    let pid = loaded_pid(name);
    if serving {
        return Ok(if pid.is_some() { Installed::AlreadyRunning } else { Installed::Deferred });
    }
    if pid.is_some() {
        return Ok(Installed::Started);
    }
    if loaded(name) {
        unload(name);
    }
    let plist = item_path(name)?;
    let out = std::process::Command::new("launchctl")
        .args(["bootstrap", &domain()])
        .arg(&plist)
        .output()
        .map_err(|e| format!("launchctl bootstrap: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "launchctl bootstrap {}: {}",
            plist.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(Installed::Started)
}

/// Take the job out of this session. The server, if launchd is running one,
/// receives SIGTERM and exits cleanly; this returns once the job is gone, so
/// a bootstrap that follows loads the plist afresh instead of finding the
/// old job still on its way out. Registration is untouched.
#[cfg(target_os = "macos")]
pub fn unload(name: &str) {
    launchctl(&["bootout", &format!("{}/{}", domain(), label(name))]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while loaded(name) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Unregister: delete the plist. A running server is left alone — it ends
/// with `stop` or at logout, and until then launchd still restarts it after a
/// crash, since it holds the job it loaded. A job with no process is booted
/// out, so nothing of the item lingers. Returns whether there was an item.
#[cfg(target_os = "macos")]
pub fn remove(name: &str) -> Result<bool, String> {
    let plist = item_path(name)?;
    if !plist.exists() {
        return Ok(false);
    }
    std::fs::remove_file(&plist).map_err(|e| format!("removing {}: {e}", plist.display()))?;
    if loaded(name) && loaded_pid(name).is_none() {
        unload(name);
    }
    Ok(true)
}

/// Whether launchd holds the job in this session — running or idle.
#[cfg(target_os = "macos")]
fn loaded(name: &str) -> bool {
    launchctl(&["print", &format!("{}/{}", domain(), label(name))])
}

/// The pid of the job's process, while launchd has one running.
#[cfg(target_os = "macos")]
fn loaded_pid(name: &str) -> Option<u32> {
    let out = std::process::Command::new("launchctl")
        .args(["print", &format!("{}/{}", domain(), label(name))])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.trim().strip_prefix("pid = ").and_then(|p| p.trim().parse().ok()))
}

#[cfg(target_os = "macos")]
fn item_path(name: &str) -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("no HOME to place the login item under")?;
    Ok(PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", label(name))))
}

// KeepAlive on failure only: a crash or a kill comes back after the throttle,
// a clean exit — `stop`, the refcount departure — stays down. The server's
// stdout and stderr land in the berth's log, which is where a crash explains
// itself.
#[cfg(target_os = "macos")]
fn item_body(name: &str, exe: &str, db: &str, log: &str, env: &[(String, String)]) -> String {
    let env = if env.is_empty() {
        String::new()
    } else {
        let pairs: String = env
            .iter()
            .map(|(k, v)| format!("\t\t<key>{}</key>\n\t\t<string>{}</string>\n", xml(k), xml(v)))
            .collect();
        format!("\t<key>EnvironmentVariables</key>\n\t<dict>\n{pairs}\t</dict>\n")
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\t<string>{label}</string>\n\
         \t<key>ProgramArguments</key>\n\t<array>\n\
         \t\t<string>{exe}</string>\n\t\t<string>{db}</string>\n\t\t<string>start</string>\n\
         \t</array>\n\
         {env}\
         \t<key>RunAtLoad</key>\n\t<true/>\n\
         \t<key>KeepAlive</key>\n\t<dict>\n\
         \t\t<key>SuccessfulExit</key>\n\t\t<false/>\n\
         \t</dict>\n\
         \t<key>ThrottleInterval</key>\n\t<integer>10</integer>\n\
         \t<key>StandardOutPath</key>\n\t<string>{log}</string>\n\
         \t<key>StandardErrorPath</key>\n\t<string>{log}</string>\n\
         \t<key>ProcessType</key>\n\t<string>Background</string>\n\
         </dict>\n\
         </plist>\n",
        label = xml(&label(name)),
        exe = xml(exe),
        db = xml(db),
        log = xml(log),
        env = env,
    )
}

#[cfg(target_os = "macos")]
fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// What a plist runs: the second of its ProgramArguments, and the
/// HARBOR_HOME among its EnvironmentVariables. Read by its keys, so a plist
/// another tool reformatted still reads.
#[cfg(target_os = "macos")]
fn parse_item(text: &str) -> Option<Item> {
    let unxml = |s: &str| {
        s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
    };
    let string_after = |key: &str, nth: usize| {
        let (_, rest) = text.split_once(key)?;
        let value = rest.split("<string>").nth(nth)?.split_once("</string>")?.0;
        Some(unxml(value))
    };
    let (_, args) = text.split_once("<key>ProgramArguments</key>")?;
    let args = &args[..args.find("</array>")?];
    let db = unxml(args.split("<string>").nth(2)?.split_once("</string>")?.0);
    Some(Item { db: db.into(), home: string_after("<key>HARBOR_HOME</key>", 1).map(PathBuf::from) })
}

// ---------------------------------------------------------------------------
// Linux — a ~/.config/systemd/user unit. `enable` registers it with
// default.target; `start` loads it now.
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn unit(name: &str) -> String {
    format!("harbor-{name}.service")
}

#[cfg(target_os = "linux")]
fn systemctl(args: &[&str]) -> Result<(), String> {
    let out = std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .map_err(|e| format!("systemctl --user {}: {e}", args.join(" ")))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "systemctl --user {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Enable the unit for login. Best effort: a build box without a user
/// session bus still gets the file.
#[cfg(target_os = "linux")]
fn register(name: &str) {
    let _ = systemctl(&["daemon-reload"]);
    if let Err(e) = systemctl(&["enable", &unit(name)]) {
        eprintln!("harbor: {e} — the unit is written, but nothing will start it at login until it is enabled");
    }
}

/// Register, enable, and start now. `serving` says whether something already
/// answers for this database; `start` on an active unit is a no-op either way.
#[cfg(target_os = "linux")]
pub fn install(db: &Path, name: &str, serving: bool) -> Result<Installed, String> {
    ready_item(db, name)?;
    if serving {
        let active = systemctl(&["is-active", "--quiet", &unit(name)]).is_ok();
        return Ok(if active { Installed::AlreadyRunning } else { Installed::Deferred });
    }
    systemctl(&["start", &unit(name)])?;
    Ok(Installed::Started)
}

/// Stop the unit's server, if systemd is running one. Registration stays.
#[cfg(target_os = "linux")]
pub fn unload(name: &str) {
    let _ = systemctl(&["stop", &unit(name)]);
}

/// Unregister: disable the unit and delete its file. A running server is
/// left alone. Returns whether there was a unit to remove.
#[cfg(target_os = "linux")]
pub fn remove(name: &str) -> Result<bool, String> {
    let path = item_path(name)?;
    if !path.exists() {
        return Ok(false);
    }
    let _ = systemctl(&["disable", &unit(name)]);
    std::fs::remove_file(&path).map_err(|e| format!("removing {}: {e}", path.display()))?;
    let _ = systemctl(&["daemon-reload"]);
    Ok(true)
}

#[cfg(target_os = "linux")]
fn item_path(name: &str) -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .ok_or("no HOME to place the login item under")?;
    Ok(base.join("systemd/user").join(unit(name)))
}

// Restart=on-failure is KeepAlive's SuccessfulExit=false: back after a crash,
// down after a clean exit. No After= on the target that wants this unit — a
// target orders itself after everything it wants, so that would be a cycle
// systemd breaks by dropping a job, usually this one.
//
// A path goes in as the bytes the filesystem holds, so everything systemd
// would read as its own syntax is escaped. Quotes keep a space through the
// word split, and inside them a backslash starts an escape. `%` is a
// specifier in every directive here (`/data/50%off.duckdb` would otherwise
// name another file, and the login item would serve a fresh, empty database
// under the real one's name). `$` is a variable in a command's arguments
// and nowhere else: the program's own path is never substituted, so a `$`
// there stays single, and doubled it would name a file that is not there.
#[cfg(target_os = "linux")]
fn item_body(name: &str, exe: &str, db: &str, log: &str, env: &[(String, String)]) -> String {
    let bare = |s: &str| s.replace('%', "%%");
    let quoted = |s: &str| bare(&s.replace('\\', "\\\\").replace('"', "\\\""));
    let word = |s: &str| quoted(s).replace('$', "$$");
    let env: String = env
        .iter()
        .map(|(k, v)| format!("Environment=\"{}={}\"\n", k, quoted(v)))
        .collect();
    format!(
        "[Unit]\n\
         Description=harbor: {name}\n\n\
         [Service]\n\
         Type=simple\n\
         {env}\
         ExecStart=\"{exe}\" \"{db}\" start\n\
         StandardOutput=append:{log}\n\
         StandardError=append:{log}\n\
         Restart=on-failure\n\
         RestartSec=10\n\n\
         [Install]\n\
         WantedBy=default.target\n",
        name = bare(name),
        exe = quoted(exe),
        db = word(db),
        log = bare(log),
        env = env,
    )
}

/// What a unit runs: the word before the `start` its ExecStart ends with,
/// and the HARBOR_HOME among its Environment assignments, each read back
/// through the escapes `item_body` writes and the bare words a unit edited
/// by hand may hold. The database is the word before `start` wherever the
/// command begins, so a unit that runs harbor through a wrapper (`env X=1
/// harbor …`) is still read, and a database given by its config name is
/// that entry's file.
#[cfg(target_os = "linux")]
fn parse_item(text: &str) -> Option<Item> {
    let (mut db, mut home) = (None, None);
    for line in text.lines().map(str::trim) {
        if let Some(command) = line.strip_prefix("ExecStart=") {
            let command = command.trim_start_matches(['-', '@', ':', '+', '!']);
            let words = words(command);
            db = words
                .iter()
                .rposition(|w| w == "start")
                .and_then(|at| words.get(at.checked_sub(1)?))
                .map(|w| w.replace("$$", "$"))
                .map(|w| match paths::looks_like_path(&w) {
                    true => w,
                    false => crate::config::load()
                        .ok()
                        .and_then(|cfg| cfg.connection.get(&paths::normalize(&w).ok()?)?.database())
                        .map_or(w, |p| p.display().to_string()),
                });
        } else if let Some(assignments) = line.strip_prefix("Environment=") {
            for word in words(assignments) {
                if let Some(v) = word.strip_prefix("HARBOR_HOME=") {
                    home = Some(PathBuf::from(v));
                }
            }
        }
    }
    Some(Item { db: db?.into(), home })
}

/// systemd's word split: words part at whitespace, a double-quoted word keeps
/// its spaces, a backslash takes the next character as it is, and `%%` is `%`.
#[cfg(target_os = "linux")]
fn words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let Some(&first) = chars.peek() else { break };
        let quoted = first == '"';
        if quoted {
            chars.next();
        }
        let mut word = String::new();
        while let Some(c) = chars.next() {
            match c {
                '\\' => word.extend(chars.next()),
                '"' if quoted => break,
                c if c.is_whitespace() && !quoted => break,
                c => word.push(c),
            }
        }
        out.push(word.replace("%%", "%"));
    }
    out
}

// ---------------------------------------------------------------------------
// Anything else — no login-item mechanism we speak.
// ---------------------------------------------------------------------------

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn arm(_db: &Path, _name: &str) -> Result<(), String> {
    Err("autostart is only supported on macOS and Linux".into())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn claimable(_db: &Path, _name: &str) -> Result<(), String> {
    Err("autostart is only supported on macOS and Linux".into())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn install(_db: &Path, _name: &str, _serving: bool) -> Result<Installed, String> {
    Err("autostart is only supported on macOS and Linux".into())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn unload(_name: &str) {}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn remove(_name: &str) -> Result<bool, String> {
    Ok(false)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn installed(_name: &str) -> bool {
    false
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn keeps(_db: &Path, _name: &str) -> bool {
    false
}

#[cfg(test)]
mod tests {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    use super::{Item, item_body, parse_item};
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    use std::path::{Path, PathBuf};

    #[cfg(target_os = "macos")]
    #[test]
    fn plist_runs_start_and_escapes_paths() {
        let body = item_body("my-db", "/opt/harbor & co/harbor", "/data/my-db.duckdb", "/tmp/log/my-db.log", &[]);
        assert!(body.contains("<string>harbor.my-db</string>"));
        assert!(body.contains("<string>/data/my-db.duckdb</string>\n\t\t<string>start</string>"));
        assert!(body.contains("<key>RunAtLoad</key>\n\t<true/>"));
        assert!(body.contains("/opt/harbor &amp; co/harbor"), "the & must be XML-escaped");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn plist_restarts_on_failure_only() {
        let body = item_body("my-db", "/opt/harbor", "/data/my-db.duckdb", "/tmp/log/my-db.log", &[]);
        assert!(body.contains("<key>KeepAlive</key>\n\t<dict>\n\t\t<key>SuccessfulExit</key>\n\t\t<false/>"));
        assert!(body.contains("<key>ThrottleInterval</key>\n\t<integer>10</integer>"));
        assert!(!body.contains("<key>KeepAlive</key>\n\t<true/>"), "a clean stop must stay stopped");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn plist_sends_output_to_the_berth_log() {
        let body = item_body("my-db", "/opt/harbor", "/data/my-db.duckdb", "/tmp/log/my-db.log", &[]);
        assert!(body.contains("<key>StandardOutPath</key>\n\t<string>/tmp/log/my-db.log</string>"));
        assert!(body.contains("<key>StandardErrorPath</key>\n\t<string>/tmp/log/my-db.log</string>"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn plist_carries_only_the_homes_that_were_set() {
        // The login item's start is a plain start: options come from
        // config.toml. The one thing that rides the environment is where
        // harbor's home is, when the shell moved it — else nothing at all.
        let bare = item_body("my-db", "/opt/harbor", "/data/my-db.duckdb", "/tmp/log/my-db.log", &[]);
        assert!(!bare.contains("EnvironmentVariables"));
        let moved = item_body("my-db", "/opt/harbor", "/data/my-db.duckdb", "/tmp/log/my-db.log",
            &[("HARBOR_HOME".into(), "/srv/h & m".into())]);
        assert!(moved.contains("<key>EnvironmentVariables</key>"));
        assert!(moved.contains("<key>HARBOR_HOME</key>\n\t\t<string>/srv/h &amp; m</string>"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unit_runs_start_and_quotes_paths() {
        let body = item_body("my-db", "/opt/harbor/harbor", "/data/my db.duckdb", "/tmp/log/my-db.log", &[]);
        assert!(body.contains("ExecStart=\"/opt/harbor/harbor\" \"/data/my db.duckdb\" start"));
        assert!(body.contains("WantedBy=default.target"));
        assert!(!body.contains("Environment="));
    }

    /// Everything systemd would read as its own syntax arrives as the bytes
    /// the filesystem holds: a specifier, a variable, a backslash, a quote.
    #[cfg(target_os = "linux")]
    #[test]
    fn unit_escapes_what_systemd_would_expand() {
        let body = item_body(
            "my-db",
            "/opt/har$bor/harbor",
            "/data/50%off \\ \"q\" $HOME.duckdb",
            "/tmp/50%/my-db.log",
            &[("HARBOR_HOME".into(), "/srv/50%\\h $x".into())],
        );
        assert!(body.contains(
            "ExecStart=\"/opt/har$bor/harbor\" \"/data/50%%off \\\\ \\\"q\\\" $$HOME.duckdb\" start\n"
        ), "{body}");
        assert!(body.contains("StandardOutput=append:/tmp/50%%/my-db.log\n"), "{body}");
        assert!(body.contains("StandardError=append:/tmp/50%%/my-db.log\n"), "{body}");
        // No variable is substituted in the program's path or in
        // Environment=, so `$` stays as it is in both.
        assert!(body.contains("Environment=\"HARBOR_HOME=/srv/50%%\\\\h $x\"\n"), "{body}");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_login_item_refuses_a_path_with_a_control_character() {
        assert!(super::printable(&[Path::new("/data/my db.duckdb"), Path::new("/data/50%off.duckdb")]).is_ok());
        let e = super::printable(&[Path::new("/data/a\nExecStartPre=/bin/x.duckdb")]).unwrap_err();
        assert!(e.contains("control character"), "{e}");
        assert!(super::printable(&[Path::new("/data/a\u{1}b.duckdb")]).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unit_restarts_on_failure_only() {
        let body = item_body("my-db", "/opt/harbor/harbor", "/data/my-db.duckdb", "/tmp/log/my-db.log", &[]);
        assert!(body.contains("Restart=on-failure"));
        assert!(body.contains("RestartSec=10"));
        assert!(!body.contains("Restart=always"), "a clean stop must stay stopped");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unit_never_orders_after_the_target_that_wants_it() {
        // default.target is After= everything it Wants=; an After=default.target
        // here would be a cycle systemd resolves by dropping a job.
        let body = item_body("my-db", "/opt/harbor/harbor", "/data/my-db.duckdb", "/tmp/log/my-db.log", &[]);
        assert!(!body.contains("After=default.target"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unit_logs_to_the_berth_log_and_carries_moved_homes() {
        let body = item_body("my-db", "/opt/harbor/harbor", "/data/my-db.duckdb", "/tmp/log/my-db.log",
            &[("XDG_STATE_HOME".into(), "/srv/state".into())]);
        assert!(body.contains("StandardOutput=append:/tmp/log/my-db.log"));
        assert!(body.contains("StandardError=append:/tmp/log/my-db.log"));
        assert!(body.contains("Environment=\"XDG_STATE_HOME=/srv/state\""));
    }

    /// An item reads back as the file and home it was written with, through
    /// every escape the writer applies.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn an_item_reads_back_what_it_runs() {
        for (db, home) in [
            ("/data/my db.duckdb", None),
            ("/data/50%off \\ \"q\" $HOME & <x>.duckdb", Some("/srv/50%\\h & $x")),
        ] {
            let env: Vec<(String, String)> = home.iter().map(|h| ("HARBOR_HOME".into(), h.to_string())).collect();
            let body = item_body("my-db", "/opt/harbor & co/harbor", db, "/tmp/log/my-db.log", &env);
            assert_eq!(
                parse_item(&body),
                Some(Item { db: PathBuf::from(db), home: home.map(PathBuf::from) }),
                "{body}"
            );
        }
    }

    /// A unit tuned by hand — bare words, extra settings, Environment with
    /// several assignments — still says which database it runs.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_hand_tuned_unit_still_reads() {
        let text = "[Service]\nEnvironment=RUST_LOG=info HARBOR_HOME=/srv/h\nLimitNOFILE=65536\n\
                    ExecStart=/home/s/.local/bin/harbor /home/s/src/medlabs/api/db/medlabs.duckdb start\n";
        assert_eq!(
            parse_item(text),
            Some(Item { db: "/home/s/src/medlabs/api/db/medlabs.duckdb".into(), home: Some("/srv/h".into()) })
        );
        // Through a wrapper, the database is still the word before `start`.
        let wrapped = "[Service]\nExecStart=/usr/bin/env RUST_LOG=1 /opt/harbor \"/d/m.duckdb\" start\n";
        assert_eq!(parse_item(wrapped), Some(Item { db: "/d/m.duckdb".into(), home: None }));
        assert_eq!(parse_item("[Service]\nType=simple\n"), None);
    }

    /// The rule that keeps a scratch database off the real item: the item is
    /// this database's only for the same file under the same home.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn an_item_is_this_databases_only_for_its_file_and_home() {
        let root = std::env::temp_dir().join(format!("hb-item-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        let root = root.canonicalize().unwrap();
        let real = Item { db: root.join("a/medlabs.duckdb"), home: None };
        assert!(real.is_for(&root.join("a/medlabs.duckdb"), None));
        // Another file with the same stem, under the same home or a scratch one.
        assert!(!real.is_for(&root.join("b/medlabs.duckdb"), None));
        assert!(!real.is_for(&root.join("b/medlabs.duckdb"), Some(Path::new("/tmp/hr"))));
        // The same file under a scratch home is not the real item's either.
        assert!(!real.is_for(&root.join("a/medlabs.duckdb"), Some(Path::new("/tmp/hr"))));
        let scratch = Item { db: root.join("b/medlabs.duckdb"), home: Some("/tmp/hr".into()) };
        assert!(scratch.is_for(&root.join("b/medlabs.duckdb"), Some(Path::new("/tmp/hr"))));
        // A home is the same home however it is spelled.
        let spelled = Item { db: root.join("b/medlabs.duckdb"), home: Some(root.join("a/../b")) };
        assert!(spelled.is_for(&root.join("b/medlabs.duckdb"), Some(&root.join("b"))));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
