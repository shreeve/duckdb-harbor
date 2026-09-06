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
//! Shared so the CLI and DuckTable arm and disarm through the same code and
//! read the same `installed` truth for a menu checkmark.

use crate::paths;
use std::path::Path;

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

/// Register the item: write the plist and clear any disable launchd may hold
/// for the label from an earlier life. Does not load it.
#[cfg(target_os = "macos")]
pub fn arm(db: &Path, name: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let canon = paths::canonical_db(db)?;
    let log = paths::log_file(&paths::runtime_dir()?, name);
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let plist = agent_path(name)?;
    if let Some(dir) = plist.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(
        &plist,
        plist_body(name, &exe.display().to_string(), &canon.display().to_string(), &log.display().to_string()),
    )
    .map_err(|e| format!("writing {}: {e}", plist.display()))?;
    launchctl(&["enable", &format!("{}/{}", domain(), label(name))]);
    Ok(())
}

/// Register and run: the server starts now under launchd and at every login.
/// `serving` says whether something already answers for this database. A job
/// launchd still holds with no process — what a clean `stop` leaves, since
/// KeepAlive only revives failures — is booted out and loaded afresh: a
/// fresh load runs at once, where a kickstart of the old job waits out
/// launchd's throttle from its last exit.
#[cfg(target_os = "macos")]
pub fn install(db: &Path, name: &str, serving: bool) -> Result<Installed, String> {
    arm(db, name)?;
    if serving {
        return Ok(if loaded(name) { Installed::AlreadyRunning } else { Installed::Deferred });
    }
    if loaded(name) {
        unload(name);
    }
    let plist = agent_path(name)?;
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

/// Unregister: delete the plist. A running server is left alone; it ends
/// with `stop` or at logout. Returns whether there was an item to remove.
#[cfg(target_os = "macos")]
pub fn remove(name: &str) -> Result<bool, String> {
    let plist = agent_path(name)?;
    if !plist.exists() {
        return Ok(false);
    }
    std::fs::remove_file(&plist).map_err(|e| format!("removing {}: {e}", plist.display()))?;
    Ok(true)
}

/// Whether a login item exists for this name — the menu checkmark's truth.
#[cfg(target_os = "macos")]
pub fn installed(name: &str) -> bool {
    agent_path(name).map(|p| p.exists()).unwrap_or(false)
}

/// Whether launchd holds the job in this session.
#[cfg(target_os = "macos")]
fn loaded(name: &str) -> bool {
    launchctl(&["print", &format!("{}/{}", domain(), label(name))])
}


#[cfg(target_os = "macos")]
fn agent_path(name: &str) -> Result<std::path::PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("no HOME to place the login item under")?;
    Ok(std::path::PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", label(name))))
}

// KeepAlive on failure only: a crash or a kill comes back after the throttle,
// a clean exit — `stop`, `.quit`, the refcount departure — stays down. The
// server's stdout and stderr land in the berth's log, which is where a crash
// explains itself.
#[cfg(target_os = "macos")]
fn plist_body(name: &str, exe: &str, db: &str, log: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\t<string>{label}</string>\n\
         \t<key>ProgramArguments</key>\n\t<array>\n\
         \t\t<string>{exe}</string>\n\t\t<string>{db}</string>\n\t\t<string>start</string>\n\
         \t</array>\n\
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
    )
}

#[cfg(target_os = "macos")]
fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
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

/// Register the unit and enable it for login. Does not start it. Enabling is
/// best effort: a build box without a user session bus still gets the file.
#[cfg(target_os = "linux")]
pub fn arm(db: &Path, name: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let canon = paths::canonical_db(db)?;
    let path = unit_path(name)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(&path, unit_body(name, &exe.display().to_string(), &canon.display().to_string()))
        .map_err(|e| format!("writing {}: {e}", path.display()))?;
    let _ = systemctl(&["daemon-reload"]);
    let _ = systemctl(&["enable", &unit(name)]);
    Ok(())
}

/// Register, enable, and start now. `serving` says whether something already
/// answers for this database; `start` on an active unit is a no-op either way.
#[cfg(target_os = "linux")]
pub fn install(db: &Path, name: &str, serving: bool) -> Result<Installed, String> {
    arm(db, name)?;
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
    let path = unit_path(name)?;
    if !path.exists() {
        return Ok(false);
    }
    let _ = systemctl(&["disable", &unit(name)]);
    std::fs::remove_file(&path).map_err(|e| format!("removing {}: {e}", path.display()))?;
    let _ = systemctl(&["daemon-reload"]);
    Ok(true)
}

/// Whether a login item exists for this name — the menu checkmark's truth.
#[cfg(target_os = "linux")]
pub fn installed(name: &str) -> bool {
    unit_path(name).map(|p| p.exists()).unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn unit_path(name: &str) -> Result<std::path::PathBuf, String> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .ok_or("no HOME to place the login item under")?;
    Ok(base.join("systemd/user").join(unit(name)))
}

// Restart=on-failure is KeepAlive's SuccessfulExit=false: back after a crash,
// down after a clean exit. Quote the two paths so a space in either survives
// systemd's word split.
#[cfg(target_os = "linux")]
fn unit_body(name: &str, exe: &str, db: &str) -> String {
    format!(
        "[Unit]\n\
         Description=harbor: {name}\n\
         After=default.target\n\n\
         [Service]\n\
         Type=simple\n\
         ExecStart=\"{exe}\" \"{db}\" start\n\
         Restart=on-failure\n\
         RestartSec=10\n\n\
         [Install]\n\
         WantedBy=default.target\n",
        exe = exe.replace('"', "\\\""),
        db = db.replace('"', "\\\""),
    )
}

// ---------------------------------------------------------------------------
// Anything else — no login-item mechanism we speak.
// ---------------------------------------------------------------------------

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn arm(_db: &Path, _name: &str) -> Result<(), String> {
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

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    #[test]
    fn plist_runs_start_and_escapes_paths() {
        let body = super::plist_body("my-db", "/opt/harbor & co/harbor", "/data/my-db.duckdb", "/tmp/log/my-db.log");
        assert!(body.contains("<string>harbor.my-db</string>"));
        assert!(body.contains("<string>/data/my-db.duckdb</string>\n\t\t<string>start</string>"));
        assert!(body.contains("<key>RunAtLoad</key>\n\t<true/>"));
        assert!(body.contains("/opt/harbor &amp; co/harbor"), "the & must be XML-escaped");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn plist_restarts_on_failure_only() {
        let body = super::plist_body("my-db", "/opt/harbor", "/data/my-db.duckdb", "/tmp/log/my-db.log");
        assert!(body.contains("<key>KeepAlive</key>\n\t<dict>\n\t\t<key>SuccessfulExit</key>\n\t\t<false/>"));
        assert!(body.contains("<key>ThrottleInterval</key>\n\t<integer>10</integer>"));
        assert!(!body.contains("<key>KeepAlive</key>\n\t<true/>"), "a clean stop must stay stopped");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn plist_sends_output_to_the_berth_log() {
        let body = super::plist_body("my-db", "/opt/harbor", "/data/my-db.duckdb", "/tmp/log/my-db.log");
        assert!(body.contains("<key>StandardOutPath</key>\n\t<string>/tmp/log/my-db.log</string>"));
        assert!(body.contains("<key>StandardErrorPath</key>\n\t<string>/tmp/log/my-db.log</string>"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn plist_carries_no_environment() {
        // The login item's start is a plain start: options come from
        // config.toml, and nothing about it is signalled through env.
        let body = super::plist_body("my-db", "/opt/harbor", "/data/my-db.duckdb", "/tmp/log/my-db.log");
        assert!(!body.contains("EnvironmentVariables"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unit_runs_start_and_quotes_paths() {
        let body = super::unit_body("my-db", "/opt/harbor/harbor", "/data/my db.duckdb");
        assert!(body.contains("ExecStart=\"/opt/harbor/harbor\" \"/data/my db.duckdb\" start"));
        assert!(body.contains("WantedBy=default.target"));
        assert!(!body.contains("Environment="));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unit_restarts_on_failure_only() {
        let body = super::unit_body("my-db", "/opt/harbor/harbor", "/data/my-db.duckdb");
        assert!(body.contains("Restart=on-failure"));
        assert!(body.contains("RestartSec=10"));
        assert!(!body.contains("Restart=always"), "a clean stop must stay stopped");
    }
}
