//! `harbor update [version] [--check] [--restart]`: the install one-liner,
//! run from inside the binary it replaces, then the word on which servers
//! still run the code they started with.
//!
//! Nothing here downloads, verifies or swaps a file. `install.sh` at the
//! repository's `main` does all of that and is the one place it is done; this
//! verb only points it at the directories this binary lives in, so a copy
//! outside `~/.local` updates itself in place, and reads the fleet
//! afterward. A running server keeps the file it opened until it is
//! restarted, and the listing shows each one's version, so the update ends
//! by naming the servers that are behind and the restart that brings each
//! forward. It restarts nothing on its own: a restart drops that server's
//! clients mid-request, and when that happens is the operator's call.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const REPO: &str = "shreeve/duckdb-harbor";
const INSTALL_SH: &str = "https://raw.githubusercontent.com/shreeve/duckdb-harbor/main/install.sh";
const INSTALL_PS1: &str = "https://raw.githubusercontent.com/shreeve/duckdb-harbor/main/install.ps1";

pub fn main(args: &[String]) -> ExitCode {
    match run(args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("harbor: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<ExitCode, String> {
    let mut wanted: Option<String> = None;
    let (mut check, mut restart) = (false, false);
    for a in args {
        match a.as_str() {
            "--check" => check = true,
            "--restart" => restart = true,
            s if s.starts_with('-') => return Err(format!("update takes a version, --check or --restart — not {s}")),
            s => {
                if wanted.is_some() {
                    return Err(format!("update takes one version — got {} and {s}", wanted.unwrap()));
                }
                wanted = Some(s.trim_start_matches('v').to_string());
            }
        }
    }
    let installed = env!("CARGO_PKG_VERSION");

    // What is newest is one HTTP HEAD away, and it decides whether there is
    // anything to do: an update to the version already here is a no-op, not
    // a download.
    let newest = newest_release()?;
    let target = wanted.clone().unwrap_or_else(|| newest.clone());
    if check {
        if newest == installed {
            println!("harbor {installed} is the newest release");
        } else {
            println!("harbor {installed} is installed; {newest} is the newest release — harbor update");
        }
        return Ok(ExitCode::SUCCESS);
    }
    if wanted.is_none() && newest == installed {
        eprintln!("harbor {installed} is the newest release");
        return report(installed, restart);
    }

    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(|e| format!("cannot find this binary: {e}"))?;
    eprintln!("harbor: {installed} -> {target}, over {}", harbor_common::paths::shorten(&exe));
    let status = installer(&exe, &target)
        .status()
        .map_err(|e| format!("cannot run the installer: {e}"))?;
    if !status.success() {
        return Err("the installer did not finish; nothing was changed unless it says so above".into());
    }

    // The binary at this path is the new one now. It, not this process,
    // knows its version.
    let now = Command::new(&exe)
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().strip_prefix("harbor ").map(str::to_string))
        .unwrap_or(target);
    report(&now, restart)
}

/// The version `releases/latest` redirects to, without the `v`. The
/// installer resolves it the same way; asking first is what lets an update
/// that would change nothing stop before downloading anything.
fn newest_release() -> Result<String, String> {
    let out = Command::new("curl")
        .args(["-fsSLI", "--retry", "3", "--retry-delay", "1", "-o"])
        .arg(if cfg!(windows) { "NUL" } else { "/dev/null" })
        .args(["-w", "%{url_effective}"])
        .arg(format!("https://github.com/{REPO}/releases/latest"))
        .output()
        .map_err(|e| format!("curl is required: {e}"))?;
    if !out.status.success() {
        return Err("cannot reach github.com".into());
    }
    let url = String::from_utf8_lossy(&out.stdout);
    let tag = url.trim().rsplit('/').next().unwrap_or("");
    match tag.strip_prefix('v') {
        Some(v) if v.chars().next().is_some_and(|c| c.is_ascii_digit()) => Ok(v.to_string()),
        _ => Err(format!("no releases found for {REPO}")),
    }
}

/// The one-liner, aimed at where this binary lives. `install.sh` takes the
/// binary's directory as `BIN` and the engine's as `LIB`; `../lib` beside a
/// `bin` directory is where the archive lays the engine out and the first
/// place the loader looks, and a binary anywhere else keeps its engine
/// beside it. Windows runs the PowerShell installer, whose layout is fixed.
fn installer(exe: &Path, version: &str) -> Command {
    let bin = exe.parent().map(Path::to_path_buf).unwrap_or_default();
    let lib = match (bin.file_name().and_then(|n| n.to_str()), bin.parent()) {
        (Some("bin"), Some(root)) if root.join("lib").is_dir() => root.join("lib"),
        _ => bin.clone(),
    };
    if cfg!(windows) {
        let mut cmd = Command::new("powershell");
        cmd.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command"]).arg(format!(
            "& ([scriptblock]::Create((irm {INSTALL_PS1}))) -Tag v{version}"
        ));
        return cmd;
    }
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(r#"curl -fsSL "$0" | bash -s -- "$@""#)
        .arg(INSTALL_SH)
        .arg(format!("v{version}"))
        .env("BIN", &bin)
        .env("LIB", &lib);
    cmd
}

/// Which running servers are not on `version`, and what to do about each.
/// With `--restart`, do it: each goes through `harbor <db> restart`, run
/// from the binary now installed, so a login-item server comes back under
/// its login item and a hand-started one as it was. As it was includes
/// where it runs: with no terminal, a hand-started server's restart serves
/// in place until SIGTERM, as `start` does for a service manager, and would
/// hold this command with it. Headless, such a server is named, not
/// restarted.
fn report(version: &str, restart: bool) -> Result<ExitCode, String> {
    let headless = !std::io::stdin().is_terminal();
    let behind: Vec<(String, PathBuf, String)> = harbor::repl::running()?
        .into_iter()
        .filter(|(_, _, v)| v != version)
        .collect();
    if behind.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    let exe = std::env::current_exe().map_err(|e| format!("cannot find this binary: {e}"))?;
    let mut failed = false;
    for (name, db, running) in &behind {
        let shown = harbor_common::paths::shorten(db);
        if restart && headless && !harbor_common::autostart::installed(name) {
            eprintln!("harbor: {name} still runs {running}, and was started by hand — restart it from a terminal: harbor {shown} restart");
            failed = true;
        } else if restart {
            eprintln!("harbor: restarting {name} ({running} -> {version})");
            let ok = Command::new(&exe)
                .arg(db)
                .arg("restart")
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            failed |= !ok;
        } else {
            eprintln!("harbor: {name} still runs {running} — harbor {shown} restart");
        }
    }
    if !restart {
        eprintln!("harbor: a running server keeps the code it started with; --restart does the above");
    }
    Ok(if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS })
}
