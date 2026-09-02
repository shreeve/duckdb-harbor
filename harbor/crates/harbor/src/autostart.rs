//! autostart — the login item that runs `harbor <db> start` when you log in.
//!
//! Installing it is just placing a file the platform's session manager already
//! reads at login: a launchd LaunchAgent on macOS, a systemd user unit on
//! Linux. It arms the NEXT login and leaves "now" to the running axis — so
//! `autostart` is attach + start + this, and `autostart stop` is attach + stop
//! + this (armed for login, off right now). Tearing it down is `detach`'s job,
//! since a login item for a database you no longer keep makes no sense.
//!
//! RunAtLoad / WantedBy, never KeepAlive: it starts the server once when you
//! log in, and does not resurrect one you stop. The server it launches is a
//! plain `start`, so it is persistent — up until you stop it or log out.

use std::path::Path;

// ---------------------------------------------------------------------------
// macOS — a ~/Library/LaunchAgents plist. Placing the file arms the next
// login; we deliberately do not `launchctl load` it, so "run now" stays the
// running axis's job and there is no double-start to reconcile.
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
pub fn install(db: &Path, name: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let canon = harbor_common::paths::canonical_db(db)?;
    let plist = agent_path(name)?;
    if let Some(dir) = plist.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(&plist, plist_body(name, &exe.display().to_string(), &canon.display().to_string()))
        .map_err(|e| format!("writing {}: {e}", plist.display()))?;
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn remove(name: &str) -> Result<bool, String> {
    let plist = agent_path(name)?;
    if !plist.exists() {
        return Ok(false);
    }
    // Best-effort unload if it happens to be loaded this session; unload reads
    // the plist by path, so it needs no uid. A not-loaded item just no-ops.
    let _ = std::process::Command::new("launchctl").arg("unload").arg(&plist).output();
    std::fs::remove_file(&plist).map_err(|e| format!("removing {}: {e}", plist.display()))?;
    Ok(true)
}

#[cfg(target_os = "macos")]
fn agent_path(name: &str) -> Result<std::path::PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("no HOME to place the login item under")?;
    Ok(std::path::PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("harbor.{name}.plist")))
}

#[cfg(target_os = "macos")]
fn plist_body(name: &str, exe: &str, db: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\t<string>harbor.{name}</string>\n\
         \t<key>ProgramArguments</key>\n\t<array>\n\
         \t\t<string>{exe}</string>\n\t\t<string>{db}</string>\n\t\t<string>start</string>\n\
         \t</array>\n\
         \t<key>RunAtLoad</key>\n\t<true/>\n\
         \t<key>ProcessType</key>\n\t<string>Background</string>\n\
         </dict>\n\
         </plist>\n",
        name = xml(name),
        exe = xml(exe),
        db = xml(db),
    )
}

#[cfg(target_os = "macos")]
fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

// ---------------------------------------------------------------------------
// Linux — a ~/.config/systemd/user unit, enabled so default.target pulls it in
// at login. `enable` (not `enable --now`) arms the next login without starting
// one now; the running axis handles now.
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
pub fn install(db: &Path, name: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let canon = harbor_common::paths::canonical_db(db)?;
    let unit = unit_path(name)?;
    if let Some(dir) = unit.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(&unit, unit_body(name, &exe.display().to_string(), &canon.display().to_string()))
        .map_err(|e| format!("writing {}: {e}", unit.display()))?;
    // Arm the next login (creates the default.target.wants symlink). Best
    // effort: a build box without a user session bus should still write the
    // unit rather than fail the whole command.
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "enable", &format!("harbor-{name}.service")])
        .output();
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn remove(name: &str) -> Result<bool, String> {
    let unit = unit_path(name)?;
    if !unit.exists() {
        return Ok(false);
    }
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "disable", &format!("harbor-{name}.service")])
        .output();
    std::fs::remove_file(&unit).map_err(|e| format!("removing {}: {e}", unit.display()))?;
    Ok(true)
}

#[cfg(target_os = "linux")]
fn unit_path(name: &str) -> Result<std::path::PathBuf, String> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .ok_or("no HOME to place the login item under")?;
    Ok(base.join("systemd/user").join(format!("harbor-{name}.service")))
}

#[cfg(target_os = "linux")]
fn unit_body(name: &str, exe: &str, db: &str) -> String {
    // Quote the two paths so a space in either survives systemd's word split.
    format!(
        "[Unit]\n\
         Description=harbor: {name}\n\
         After=default.target\n\n\
         [Service]\n\
         Type=simple\n\
         ExecStart=\"{exe}\" \"{db}\" start\n\n\
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
pub fn install(_db: &Path, _name: &str) -> Result<(), String> {
    Err("autostart is only supported on macOS and Linux".into())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn remove(_name: &str) -> Result<bool, String> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn plist_names_the_verb_and_escapes_paths() {
        let body = plist_body("my-db", "/opt/harbor & co/harbor", "/data/my-db.duckdb");
        assert!(body.contains("<string>harbor.my-db</string>"));
        assert!(body.contains("<string>start</string>"));
        assert!(body.contains("<key>RunAtLoad</key>"));
        assert!(!body.contains("<key>KeepAlive</key>"), "RunAtLoad, never KeepAlive");
        assert!(body.contains("/opt/harbor &amp; co/harbor"), "the & must be XML-escaped");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unit_names_the_verb_and_quotes_paths() {
        let body = unit_body("my-db", "/opt/harbor/harbor", "/data/my db.duckdb");
        assert!(body.contains("ExecStart=\"/opt/harbor/harbor\" \"/data/my db.duckdb\" start"));
        assert!(body.contains("WantedBy=default.target"));
        assert!(!body.contains("Restart="), "no KeepAlive equivalent");
    }
}
