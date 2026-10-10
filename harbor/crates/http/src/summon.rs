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
/// terminal, stdout and stderr appended to its log, so a failure has a face.
/// `ephemeral` is the private lifetime signal: a summoned server is
/// refcounted and leaves when its last client does. It rides the
/// environment, not the command line, since a spawn is not a verb the user
/// typed.
///
/// A server that is leaving still holds the database's lock for a moment
/// after it stops answering, and a server started then loses the lock to it
/// and exits. That start is made again until the deadline.
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
    let log_path = log_of(sock)?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let from = std::fs::metadata(&log_path).map_or(0, |m| m.len());
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
        let outcome = loop {
            if ready(&transport) {
                break Ok(());
            }
            // The child ending is an answer, not a timeout, but it can be
            // the good answer: two clients raced, this one lost the database
            // lock to the winner, and the winner's socket (the same path)
            // serves it fine. An ended child and no listener is a failure,
            // unless the lock it lost belongs to a server on its way out.
            if let Ok(Some(status)) = child.try_wait() {
                break match ready(&transport) {
                    true => Ok(()),
                    false if lost_lock(&log_path, from) && Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(100));
                        Err(None)
                    }
                    false => Err(Some(format!("the server did not start ({status}) — {}", log_tail(&log_path)))),
                };
            }
            if Instant::now() >= deadline {
                break Err(Some(format!("{} did not come up in 15s — {}", db.display(), log_tail(&log_path))));
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let _ = std::thread::Builder::new().name("harbor-reaper".into()).spawn(move || child.wait());
        match outcome {
            Ok(()) => return Ok(()),
            Err(Some(failure)) => return Err(failure),
            Err(None) => {}
        }
    }
}

/// Past this a summoned server's log starts again at its next start rather
/// than growing for as long as the database is used.
const LOG_CAP: u64 = 1 << 20;

/// The log a summoned server writes: `log/<socket's name>.log` in the
/// runtime directory, beside the logs of login items, which are named for
/// their database. Made private before it is written, and emptied at a
/// start once it has passed `LOG_CAP`.
fn log_of(sock: &Path) -> Result<std::path::PathBuf, String> {
    let runtime = sock.parent().ok_or_else(|| format!("{}: no runtime directory", sock.display()))?;
    harbor_common::perms::ensure_private_dir(runtime)?;
    let stem = sock.file_stem().unwrap_or_default().to_string_lossy();
    let log = harbor_common::paths::log_file(runtime, &stem);
    if let Some(dir) = log.parent() {
        harbor_common::perms::ensure_private_dir(dir)?;
    }
    if std::fs::metadata(&log).is_ok_and(|m| m.len() > LOG_CAP) {
        std::fs::write(&log, "").map_err(|e| format!("{}: {e}", log.display()))?;
    }
    Ok(log)
}

/// Whether the start that wrote the log past byte `from` lost the database's
/// lock to another harbor, which the engine's refusal names: one that does
/// not answer on the socket is on its way out. A lock held by any other
/// program is not waited for.
fn lost_lock(log_path: &Path, from: u64) -> bool {
    let mut said = String::new();
    if let Ok(mut file) = std::fs::File::open(log_path)
        && file.seek(SeekFrom::Start(from)).is_ok()
    {
        let _ = file.read_to_string(&mut said);
    }
    said.split("Conflicting lock is held in ")
        .nth(1)
        .and_then(|rest| rest.split(" (PID").next())
        .and_then(|holder| Path::new(holder).file_name())
        .is_some_and(|program| program.to_string_lossy().starts_with("harbor"))
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
        // Its log is under the runtime directory's `log/`, named for the socket.
        assert!(dir.join("log/x.log").exists());
        let missing = summon(dir.join("absent"), Path::new("/data/x.duckdb"), &sock, &[], true).unwrap_err();
        assert!(missing.starts_with("cannot run"), "{missing}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_start_that_lost_the_lock_to_a_leaving_server_is_made_again() {
        let dir = std::env::temp_dir().join(format!("hh-relaunch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("x.sock");
        // A stand-in that loses the lock twice, then stays up while the
        // test answers on its socket for it.
        let (exe, tries, up) = (dir.join("harbor"), dir.join("tries"), dir.join("up"));
        let script = format!(
            "#!/bin/sh\necho x >> {tries}\n\
             if [ $(wc -l < {tries}) -le 2 ]; then echo 'IO Error: Conflicting lock is held in /opt/bin/harbor (PID 7) by user u' >&2; exit 1; fi\n\
             touch {up}; sleep 3\n",
            tries = tries.display(),
            up = up.display(),
        );
        std::fs::write(&exe, script).unwrap();
        harbor_common::perms::chmod(&exe, 0o755).unwrap();
        let serving = sock.clone();
        std::thread::spawn(move || {
            while !up.exists() {
                std::thread::sleep(Duration::from_millis(20));
            }
            let listener = std::os::unix::net::UnixListener::bind(&serving).unwrap();
            for stream in listener.incoming().flatten() {
                use std::io::{BufRead, Write};
                let mut head = std::io::BufReader::new(&stream);
                let mut line = String::new();
                while head.read_line(&mut line).is_ok_and(|n| n > 2) {
                    line.clear();
                }
                let _ = (&stream).write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            }
        });
        summon(&exe, Path::new("/data/x.duckdb"), &sock, &[], true).unwrap();
        assert_eq!(std::fs::read_to_string(&tries).unwrap().lines().count(), 3);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_a_lock_another_harbor_holds_is_waited_for() {
        let path = std::env::temp_dir().join(format!("hh-lock-{}.log", std::process::id()));
        let refusal = |holder: &str| {
            format!("harbor: open x: IO Error: Could not set lock on file \"x\": Conflicting lock is held in {holder} (PID 9) by user u.\n")
        };
        std::fs::write(&path, refusal("/usr/local/bin/harbor")).unwrap();
        assert!(lost_lock(&path, 0));
        std::fs::write(&path, refusal("/opt/homebrew/bin/duckdb")).unwrap();
        assert!(!lost_lock(&path, 0));
        // Only what this start wrote is read, not an earlier start's refusal.
        let old = refusal("/usr/local/bin/harbor");
        std::fs::write(&path, format!("{old}harbor: serving x\n")).unwrap();
        assert!(!lost_lock(&path, old.len() as u64));
        std::fs::remove_file(&path).unwrap();
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
