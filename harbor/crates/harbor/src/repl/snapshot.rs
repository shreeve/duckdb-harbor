//! A snapshot whose lifetime follows the backup client, including file work
//! between SQL requests. Losing its renewal aborts rather than reopening it.

use super::{Conn, Mode, Outcome, RenderOpts, endpoint, http, resolve, run_sql_in_session};
use std::io;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Clone, Default)]
pub(super) struct Health(Arc<Mutex<Option<String>>>);

impl Health {
    pub(super) fn check(&self) -> io::Result<()> {
        match self.0.lock().unwrap().as_ref() {
            Some(message) => Err(io::Error::other(message.clone())),
            None => Ok(()),
        }
    }
}

struct Heartbeat {
    stop: mpsc::Sender<()>,
    thread: Option<JoinHandle<()>>,
    health: Health,
}

impl Heartbeat {
    fn start(conn: &Conn, id: &str, ttl: Duration) -> Result<Self, String> {
        if ttl < Duration::from_millis(30) {
            return Err("backup renewal window is too short".into());
        }
        let (stop, rx) = mpsc::channel();
        let health = Health::default();
        let failure = health.clone();
        let transport = conn.transport.clone();
        let route = endpoint::session_renew(id);
        let interval = ttl / 3;
        let timeout = interval.min(Duration::from_secs(5));
        let thread = thread::Builder::new()
            .name("backup-heartbeat".into())
            .spawn(move || {
                while let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(interval) {
                    let deadline = Instant::now() + timeout;
                    let tick = || {
                        if Instant::now() >= deadline {
                            Err(io::Error::new(io::ErrorKind::TimedOut, "renewal timed out"))
                        } else {
                            Ok(())
                        }
                    };
                    let result = http::request_streaming(&transport, &route, None, &tick).and_then(
                        |response| {
                            tick()?;
                            if response.status == 200 {
                                Ok(())
                            } else {
                                Err(io::Error::other(format!("HTTP {}", response.status)))
                            }
                        },
                    );
                    if let Err(e) = result {
                        *failure.0.lock().unwrap() = Some(format!(
                            "backup lease renewal failed: {e}; the snapshot was abandoned"
                        ));
                        break;
                    }
                }
            })
            .map_err(|e| format!("starting backup heartbeat: {e}"))?;
        Ok(Self {
            stop,
            thread: Some(thread),
            health,
        })
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Release<'a>(&'a Conn, String);

impl Drop for Release<'_> {
    fn drop(&mut self) {
        // DELETE cancels active work and rolls back on errors and unwind.
        let _ = http::request(
            &self.0.transport,
            &endpoint::session(&self.1),
            None,
            Some(Duration::from_secs(5)),
        );
    }
}

/// Run a multi-pass export on one renewable snapshot. Heartbeats continue
/// during both SQL and the closure's file inspection. A lost lease or failed
/// statement aborts; the operation never resumes on a newer snapshot.
pub fn with_snapshot<T>(
    target: &str,
    work: impl FnOnce(&dyn Fn(&str) -> Result<(), String>) -> Result<T, String>,
) -> Result<T, String> {
    let (conn, _) = resolve(target, &[])?;
    let _anchor = http::hold(&conn.transport);
    let response = http::request(
        &conn.transport,
        &endpoint::SESSIONS_CREATE,
        Some(r#"{"purpose":"backup"}"#),
        Some(Duration::from_secs(10)),
    )
    .map_err(|e| format!("opening backup session: {e}"))?;
    let status = response.status;
    let text = response
        .body_string()
        .map_err(|e| format!("reading backup session: {e}"))?;
    if status != 200 {
        return Err(format!("opening backup session: HTTP {status}: {text}"));
    }
    let lease: wire::SessionNewResponse =
        serde_json::from_str(&text).map_err(|e| format!("invalid backup session response: {e}"))?;
    // Construct this first: stop the heartbeat before releasing the lease.
    let release = Release(&conn, lease.session_id);
    if lease.purpose != Some(wire::SessionPurpose::Backup) {
        return Err(
            "server does not support renewable backup sessions; upgrade the Harbor server".into(),
        );
    }
    let heartbeat = Heartbeat::start(&conn, &release.1, Duration::from_millis(lease.ttl_ms))?;
    let opts = RenderOpts {
        mode: Mode::Trash,
        ..RenderOpts::default()
    };
    let execute = |sql: &str| {
        heartbeat.health.check().map_err(|e| e.to_string())?;
        let outcome =
            run_sql_in_session(&conn, sql, &opts, Some(&release.1), Some(&heartbeat.health));
        heartbeat.health.check().map_err(|e| e.to_string())?;
        match outcome {
            Outcome::Done => Ok(()),
            Outcome::Cancelled => Err("backup interrupted".into()),
            Outcome::Failed => Err("backup statement failed; the snapshot was abandoned".into()),
        }
    };
    execute("BEGIN")?;
    let value = work(&execute)?;
    execute("COMMIT")?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Server {
        target: String,
        log: Arc<Mutex<Vec<String>>>,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl Server {
        fn new(supports_backup: bool, renew_status: u16) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let target = format!("http://{}", listener.local_addr().unwrap());
            listener.set_nonblocking(true).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let log = Arc::new(Mutex::new(Vec::new()));
            let stopping = stop.clone();
            let requests = log.clone();
            let thread = thread::spawn(move || {
                let mut handlers = Vec::new();
                while !stopping.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let requests = requests.clone();
                            handlers.push(thread::spawn(move || {
                                serve(stream, requests, supports_backup, renew_status);
                            }));
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(e) => panic!("accept: {e}"),
                    }
                }
                for handler in handlers {
                    handler.join().unwrap();
                }
            });
            Self {
                target,
                log,
                stop,
                thread: Some(thread),
            }
        }

        fn requests(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            self.thread.take().unwrap().join().unwrap();
        }
    }

    fn serve(stream: TcpStream, log: Arc<Mutex<Vec<String>>>, supported: bool, renew: u16) {
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        // The anchor deliberately sends no HTTP request.
        if !matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
            return;
        }
        let route = line
            .split_whitespace()
            .take(2)
            .collect::<Vec<_>>()
            .join(" ");
        let mut len = 0;
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some((key, value)) = line.split_once(':')
                && key.eq_ignore_ascii_case("content-length")
            {
                len = value.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; len];
        reader.read_exact(&mut body).unwrap();
        let (status, body) = if route == "POST /sql/sessions" {
            let request: wire::SessionNewRequest = serde_json::from_slice(&body).unwrap();
            assert_eq!(request.purpose, Some(wire::SessionPurpose::Backup));
            let purpose = if supported {
                r#", "purpose":"backup""#
            } else {
                ""
            };
            (
                200,
                format!(r#"{{"sessionId":"test","ttlMs":300,"idleTtlMs":0{purpose}}}"#),
            )
        } else if route.ends_with("/renew") {
            (renew, r#"{"renewed":true}"#.into())
        } else if route == "POST /sql" {
            let request: wire::SqlRequest = serde_json::from_slice(&body).unwrap();
            assert_eq!(request.session_id.as_deref(), Some("test"));
            log.lock().unwrap().push(request.sql.clone());
            if request.sql == "STALL" {
                // No headers: only a failed heartbeat can unblock this client.
                let _ = reader.read(&mut [0]);
                return;
            }
            (
                200,
                "{\"type\":\"end\",\"rowCount\":0,\"timeMs\":0}\n".into(),
            )
        } else {
            assert_eq!(route, "DELETE /sql/sessions/test");
            (200, r#"{"released":true}"#.into())
        };
        log.lock().unwrap().push(route);
        let mut stream = reader.into_inner();
        let _ = write!(
            stream,
            "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    }

    #[test]
    fn renews_during_file_work_and_stops_before_release() {
        let server = Server::new(true, 200);
        with_snapshot(&server.target, |_| {
            thread::sleep(Duration::from_millis(850));
            Ok(())
        })
        .unwrap();
        let log = server.requests();
        assert!(
            log.iter().filter(|r| r.ends_with("/renew")).count() >= 3,
            "{log:?}"
        );
        assert!(log.contains(&"BEGIN".into()) && log.contains(&"COMMIT".into()));
        assert_eq!(log.last().unwrap(), "DELETE /sql/sessions/test");
        thread::sleep(Duration::from_millis(150));
        assert_eq!(log, server.requests());
    }

    #[test]
    fn failed_renewal_aborts_before_headers_and_never_commits() {
        let server = Server::new(true, 404);
        let start = Instant::now();
        let error = with_snapshot(&server.target, |execute| execute("STALL")).unwrap_err();
        assert!(
            error.contains("renewal failed"),
            "{error}; {:?}",
            server.requests()
        );
        assert!(start.elapsed() < Duration::from_secs(2));
        let log = server.requests();
        assert!(log.contains(&"STALL".into()));
        assert!(!log.contains(&"COMMIT".into()));
        assert_eq!(log.last().unwrap(), "DELETE /sql/sessions/test");
    }

    #[test]
    fn old_server_is_rejected_and_its_lease_released() {
        let server = Server::new(false, 200);
        let error = with_snapshot(&server.target, |_| -> Result<(), String> {
            panic!("must not export on a legacy lease");
        })
        .unwrap_err();
        assert!(error.contains("upgrade"), "{error}");
        assert_eq!(
            server.requests(),
            ["POST /sql/sessions", "DELETE /sql/sessions/test"]
        );
    }
}
