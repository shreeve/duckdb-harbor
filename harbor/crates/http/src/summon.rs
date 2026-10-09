//! Starting a server for a database file, as both clients do it.

use crate::{Transport, ready};
use std::ffi::OsStr;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Start `exe <db> start [args]` and wait until it answers on `sock`, the
/// socket a server of `db` listens on. Detached: its own process group, no
/// terminal, stdout and stderr appended to the log beside the socket, so a
/// failure has a face. `ephemeral` is the private lifetime signal: a
/// summoned server is refcounted and leaves when its last client does. It
/// rides the environment, not the command line, since a spawn is not a verb
/// the user typed.
///
/// A headless `start` serves in the process it is, so the server is this
/// process's child for as long as it runs. A thread of its own waits for
/// it, so it never lingers as a zombie.
pub fn summon(
    exe: impl AsRef<OsStr>,
    db: &Path,
    sock: &Path,
    args: &[String],
    ephemeral: bool,
) -> Result<(), String> {
    let exe = exe.as_ref();
    let transport = Transport::Unix(sock.to_path_buf());
    if let Some(runtime) = sock.parent() {
        harbor_common::perms::ensure_private_dir(runtime)?;
    }
    let log_path = sock.with_extension("log");
    let log = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&log_path)
        .map_err(|e| format!("{}: {e}", log_path.display()))?;
    let mut cmd = Command::new(exe);
    cmd.arg(db)
        .arg("start")
        .args(args)
        .stdin(Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log)
        .process_group(0);
    if ephemeral {
        cmd.env("HARBOR_EPHEMERAL", "1");
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot run {} (is harbor installed?): {e}", Path::new(exe).display()))?;

    let deadline = Instant::now() + Duration::from_secs(15);
    let outcome = loop {
        if ready(&transport) {
            break Ok(());
        }
        // The child ending is an answer, not a timeout, but it can be the
        // good answer: two clients raced, this one lost the database lock to
        // the winner, and the winner's socket (the same path) serves it fine.
        // Only an ended child and no listener is a failure.
        if let Ok(Some(status)) = child.try_wait() {
            return match ready(&transport) {
                true => Ok(()),
                false => Err(format!("the server did not start ({status}) — {}", log_tail(&log_path))),
            };
        }
        if Instant::now() >= deadline {
            break Err(format!("{} did not come up in 15s — {}", db.display(), log_tail(&log_path)));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let _ = std::thread::Builder::new().name("harbor-reaper".into()).spawn(move || child.wait());
    outcome
}

/// The log's last few lines, inlined: nobody should have to go and find
/// the file to learn why a server never came up.
fn log_tail(log_path: &Path) -> String {
    let mut text = String::new();
    if let Ok(mut file) = std::fs::File::open(log_path) {
        let _ = file.seek(SeekFrom::End(-4096)).or_else(|_| file.seek(SeekFrom::Start(0)));
        let mut bytes = Vec::new();
        let _ = file.read_to_end(&mut bytes);
        text = String::from_utf8_lossy(&bytes).into_owned();
    }
    if text.trim().is_empty() {
        return format!("see {}", log_path.display());
    }
    let mut tail: Vec<&str> = text.lines().rev().take(3).collect();
    tail.reverse();
    format!("its log says:\n        {}", tail.join("\n        "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_that_cannot_start_says_why() {
        let dir = std::env::temp_dir().join(format!("hh-summon-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("x.sock");
        // A stand-in for harbor that says why it will not serve and leaves.
        let exe = dir.join("harbor");
        std::fs::write(&exe, "#!/bin/sh\necho \"refusing $1 $2\" >&2\nexit 3\n").unwrap();
        harbor_common::perms::chmod(&exe, 0o755).unwrap();
        let err = summon(&exe, Path::new("/data/x.duckdb"), &sock, &[], true).unwrap_err();
        assert!(err.contains("did not start"), "{err}");
        assert!(err.contains("refusing /data/x.duckdb start"), "{err}");
        let missing = summon(dir.join("absent"), Path::new("/data/x.duckdb"), &sock, &[], true).unwrap_err();
        assert!(missing.starts_with("cannot run"), "{missing}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_log_tail_is_its_last_three_lines() {
        let path = std::env::temp_dir().join(format!("hh-tail-{}.log", std::process::id()));
        let long: String = (0..2000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&path, long).unwrap();
        assert_eq!(log_tail(&path), "its log says:\n        line 1997\n        line 1998\n        line 1999");
        std::fs::write(&path, "\n").unwrap();
        assert!(log_tail(&path).starts_with("see "));
        std::fs::remove_file(&path).unwrap();
    }
}
