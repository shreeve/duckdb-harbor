//! A snapshot whose lifetime follows the backup client, including file work
//! between SQL requests. Losing its renewal aborts rather than reopening it.

use super::{Mode, Outcome, RenderOpts, Transport, http, resolve, run_sql};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

/// The backup session renewed every third of its window until this is
/// dropped. The first renewal that fails marks `health`, which ends the
/// statement under way and every one after it, and the renewals stop.
fn heartbeat(transport: &Transport, id: &str, ttl: Duration, health: &Health) -> Result<http::Beat, String> {
    if ttl < Duration::from_millis(30) {
        return Err("backup renewal window is too short".into());
    }
    let (transport, id, failure) = (transport.clone(), id.to_string(), health.clone());
    http::beat("backup-heartbeat", ttl / 3, move || {
        let why = match http::session_renew(&transport, &id) {
            Ok(true) => return Some(ttl / 3),
            Ok(false) => "the server renews no such session".to_string(),
            Err(e) => e.to_string(),
        };
        *failure.0.lock().unwrap() = Some(format!("backup lease renewal failed: {why}; the snapshot was abandoned"));
        None
    })
    .map_err(|e| format!("starting backup heartbeat: {e}"))
}

/// Releasing the session cancels its work and rolls it back, on an error
/// and on unwind as well as at the end.
struct Release<'a>(&'a Transport, String);

impl Drop for Release<'_> {
    fn drop(&mut self) {
        let _ = http::session_release(self.0, &self.1);
    }
}

/// Run a multi-pass export on one renewable snapshot. Heartbeats continue
/// during both SQL and the closure's file inspection. A lost lease or failed
/// statement aborts; the operation never resumes on a newer snapshot.
pub fn with_snapshot<T>(
    target: &str,
    work: impl FnOnce(&dyn Fn(&str) -> Result<(), String>) -> Result<T, String>,
) -> Result<T, String> {
    let (transport, _) = resolve(target, &[])?;
    let _anchor = http::hold(&transport);
    // Ctrl-C cancels the statement under way and fails the work, and the
    // caller takes back whatever it had written. A statement that runs to
    // its end regardless is still the last one: the interrupt is kept here
    // as well, since the cancel's own flag is spent on asking.
    super::cancel_on_interrupt();
    let interrupted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, interrupted.clone());
    let interrupted = || interrupted.load(std::sync::atomic::Ordering::Relaxed);
    let ask = wire::SessionNewRequest { purpose: Some(wire::SessionPurpose::Backup), ..Default::default() };
    let lease = http::session_open(&transport, &ask).map_err(|e| format!("opening backup session: {e}"))?;
    // Made before the heartbeat, so it drops after it: the renewals stop
    // before the lease is released.
    let release = Release(&transport, lease.session_id);
    if lease.purpose != Some(wire::SessionPurpose::Backup) {
        return Err("server does not support renewable backup sessions; upgrade the Harbor server".into());
    }
    let health = Health::default();
    let _heartbeat = heartbeat(&transport, &release.1, Duration::from_millis(lease.ttl_ms), &health)?;
    let opts = RenderOpts { mode: Mode::Trash, ..RenderOpts::default() };
    let execute = |sql: &str| {
        health.check().map_err(|e| e.to_string())?;
        let (outcome, _) = run_sql(&transport, sql, &opts, Some(&release.1), Some(&health));
        health.check().map_err(|e| e.to_string())?;
        match outcome {
            Outcome::Cancelled => Err("backup interrupted".into()),
            Outcome::Done | Outcome::Closed if interrupted() => Err("backup interrupted".into()),
            Outcome::Done | Outcome::Closed => Ok(()),
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
    use std::thread::{self, JoinHandle};
    use std::time::Instant;
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
        // The anchor speaks once, to mark its connection as having spoken, and
        // then holds it. That is the lifetime of the client, not a step of the
        // backup, so it is answered and left out of the conversation below.
        let (status, body) = if route == "GET /ready" {
            (200, "{}".to_string())
        } else if route == "POST /sql/sessions" {
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
        if route != "GET /ready" {
            log.lock().unwrap().push(route);
        }
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
    fn a_server_without_renewable_sessions_is_refused_and_its_lease_released() {
        let server = Server::new(false, 200);
        let error = with_snapshot(&server.target, |_| -> Result<(), String> {
            panic!("must not export on a lease that cannot be renewed");
        })
        .unwrap_err();
        assert!(error.contains("upgrade"), "{error}");
        assert_eq!(
            server.requests(),
            ["POST /sql/sessions", "DELETE /sql/sessions/test"]
        );
    }
}
