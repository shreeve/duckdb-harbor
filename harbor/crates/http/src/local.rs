//! The servers on this machine, as both clients find and stop them: by the
//! sockets they listen on in the runtime directory.

use crate::{Transport, ready, request};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use wire::endpoint;

/// A socket in the runtime directory that answered.
pub struct Found {
    pub sock: PathBuf,
    /// Its `/info`, or `None` when it answered with something else: alive,
    /// and saying nothing about what it serves.
    pub info: Option<serde_json::Value>,
}

/// Every server listening in `runtime`, in socket order: `readdir` for
/// `*.sock`, and `GET /info` on each. The listening socket is the
/// registration, so a socket nothing listens on is litter, not state, and
/// `sweep` unlinks it. Only on proof: a refusal, and a second one a beat
/// later, since a server whose listen queue is full refuses for a moment
/// too. Any other error proves nothing, and an unlink on nothing is how a
/// live server loses its front door.
pub fn discover(runtime: &Path, sweep: bool) -> Vec<Found> {
    let Ok(entries) = std::fs::read_dir(runtime) else { return Vec::new() };
    let mut socks: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "sock"))
        .collect();
    socks.sort();
    let info = |t: &Transport| request(t, &endpoint::INFO, None, Some(Duration::from_secs(2)));
    let mut found = Vec::new();
    for sock in socks {
        let transport = Transport::Unix(sock.clone());
        match info(&transport) {
            Ok(r) if r.status == 200 => {
                let info = r.body_string().ok().and_then(|body| serde_json::from_str(body.trim()).ok());
                found.push(Found { sock, info });
            }
            Ok(_) => found.push(Found { sock, info: None }),
            Err(e) if sweep && e.kind() == ErrorKind::ConnectionRefused => {
                std::thread::sleep(Duration::from_millis(200));
                if info(&transport).is_err_and(|e| e.kind() == ErrorKind::ConnectionRefused) {
                    let _ = std::fs::remove_file(&sock);
                }
            }
            Err(_) => {}
        }
    }
    found
}

/// Stop the server listening on `sock` and wait until it has gone. A server
/// unlinks its socket as the last thing before it exits and lets go of the
/// database's lock, so a start that follows meets a free file. `Ok(false)`
/// when nothing answered there.
pub fn shutdown(sock: &Path) -> Result<bool, String> {
    let transport = Transport::Unix(sock.to_path_buf());
    if !ready(&transport) {
        return Ok(false);
    }
    // The server can close the connection as it goes, so a request that
    // fails is a failure only while the server still answers.
    if let Err(e) = request(&transport, &endpoint::SHUTDOWN, None, Some(Duration::from_secs(30)))
        && ready(&transport)
    {
        return Err(e.to_string());
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    while sock.exists() || ready(&transport) {
        if Instant::now() > deadline {
            return Err("it is still shutting down after 60s".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;

    /// Answer each connection on `listener` with the next of `replies`.
    fn serve(listener: UnixListener, replies: &'static [&'static str]) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            for reply in replies {
                let (mut stream, _) = listener.accept().unwrap();
                let mut head = Vec::new();
                let mut byte = [0_u8; 1];
                while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap() == 1 {
                    head.push(byte[0]);
                }
                stream.write_all(reply.as_bytes()).unwrap();
            }
        })
    }

    #[test]
    fn discovery_tells_a_server_from_a_mute_one_and_sweeps_only_on_proof() {
        // /tmp, not $TMPDIR: a socket path has 104 bytes.
        let dir = PathBuf::from(format!("/tmp/hh-found-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let info = UnixListener::bind(dir.join("a.sock")).unwrap();
        let mute = UnixListener::bind(dir.join("b.sock")).unwrap();
        drop(UnixListener::bind(dir.join("c.sock")).unwrap()); // a leftover: nothing listens
        std::fs::write(dir.join("d.log"), "").unwrap();
        let servers = [
            serve(info, &["HTTP/1.1 200 OK\r\nContent-Length: 12\r\n\r\n{\"name\":\"a\"}", "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}"]),
            serve(mute, &["HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n"; 2]),
        ];

        let found = discover(&dir, false);
        assert_eq!(found.iter().map(|f| f.sock.clone()).collect::<Vec<_>>(), [dir.join("a.sock"), dir.join("b.sock")]);
        assert_eq!(found[0].info.as_ref().unwrap()["name"], "a");
        assert!(found[1].info.is_none());
        assert!(dir.join("c.sock").exists(), "a read-only look unlinks nothing");

        assert_eq!(discover(&dir, true).len(), 2);
        assert!(!dir.join("c.sock").exists(), "a socket refused twice is litter");
        assert!(dir.join("d.log").exists());
        for server in servers {
            server.join().unwrap();
        }
        assert!(discover(&dir.join("absent"), true).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_shutdown_waits_until_the_server_has_gone() {
        let dir = PathBuf::from(format!("/tmp/hh-stop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("x.sock");
        assert_eq!(shutdown(&sock), Ok(false), "nothing to stop");

        // A server that answers /ready, takes the shutdown, and then takes a
        // moment to leave, unlinking its socket last.
        let listener = UnixListener::bind(&sock).unwrap();
        let replies = &["HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n", "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n"];
        let (server, gone) = (serve(listener, replies), sock.clone());
        let leaving = std::thread::spawn(move || {
            server.join().unwrap();
            std::thread::sleep(Duration::from_millis(300));
            std::fs::remove_file(gone).unwrap();
        });
        let began = Instant::now();
        assert_eq!(shutdown(&sock), Ok(true));
        assert!(began.elapsed() >= Duration::from_millis(300) && !sock.exists());
        leaving.join().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
