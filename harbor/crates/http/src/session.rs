//! Sessions: a connection Harbor pins to one client so a transaction can
//! span requests, and the touch that keeps one past its idle limit.

use crate::{Beat, Failure, Transport, beat, call, request};
use std::sync::Mutex;
use std::time::Duration;
use wire::{SessionNewRequest, SessionNewResponse, SqlRequest, code, endpoint};

/// Open a session. Released with [`session_release`], which rolls back
/// whatever it left open, so an abandoned session never half-commits.
pub fn session_open(transport: &Transport, ask: &SessionNewRequest) -> Result<SessionNewResponse, Failure> {
    let body = serde_json::to_string(ask).expect("a session request serializes");
    call(transport, &endpoint::SESSIONS_CREATE, Some(&body), Duration::from_secs(10))
}

/// Release a session, rolling back any open transaction. Releasing one that
/// is gone is not an error: it answers `released: false`.
pub fn session_release(transport: &Transport, id: &str) -> Result<wire::ReleasedResponse, Failure> {
    call(transport, &endpoint::session(id), None, Duration::from_secs(10))
}

/// Renew a session: a backup's window, or an ordinary session's idle clock,
/// whose five-minute ceiling stays where it was. It runs nothing, so it is
/// answered while a statement runs on the session. `Ok(false)` is a server
/// that renews only backup sessions, which a statement keeps alive instead.
pub fn session_renew(transport: &Transport, id: &str) -> Result<bool, Failure> {
    match call::<serde_json::Value>(transport, &endpoint::session_renew(id), None, Duration::from_secs(5)) {
        Ok(_) => Ok(true),
        // Such a server answers 400, and one older still has no route: 404
        // `not_found`, which is not the 404 `no_such_session` of a session
        // that is gone.
        Err(Failure::Refused { code, .. }) if code == code::BAD_REQUEST || code == code::NOT_FOUND => {
            Ok(false)
        }
        Err(other) => Err(other),
    }
}

/// The servers that renew only backup sessions, so `session_touch` asks
/// each of them once.
static RENEWS_ONLY_BACKUPS: Mutex<Vec<Transport>> = Mutex::new(Vec::new());

/// Keep a session's idle clock from running out: renew it, or, on a server
/// that renews only backup sessions, run `SELECT 1` on it. A session busy
/// with a statement is alive, and is reported so.
pub fn session_touch(transport: &Transport, id: &str) -> Result<(), Failure> {
    let known = || RENEWS_ONLY_BACKUPS.lock().unwrap_or_else(|p| p.into_inner());
    if !known().contains(transport) {
        if session_renew(transport, id)? {
            return Ok(());
        }
        known().push(transport.clone());
    }
    let body = serde_json::to_string(&SqlRequest {
        sql: "SELECT 1".to_string(),
        session_id: Some(id.to_string()),
        ..Default::default()
    })
    .expect("a request serializes");
    let response = request(transport, &endpoint::SQL, Some(&body), Some(Duration::from_secs(5)))?;
    let status = response.status;
    let text = response.body_string()?;
    if (200..300).contains(&status) {
        return Ok(());
    }
    match Failure::of(status, &text) {
        Failure::Refused { code, .. } if code == code::SESSION_BUSY => Ok(()),
        failure => Err(failure),
    }
}

/// A session touched every third of its idle limit, so the server never
/// reclaims it while this client lives; when the client dies the touches
/// stop and the server reclaims it as it would any other. Its fixed ceiling
/// is untouched by this, and still ends it. Stops once the session is gone,
/// and when dropped. `None` for a session with no idle limit.
pub fn keep_alive(transport: &Transport, id: &str, idle: Duration) -> Option<Beat> {
    if idle.is_zero() {
        return None;
    }
    let (transport, id) = (transport.clone(), id.to_string());
    beat("harbor-keepalive", idle / 3, move || match session_touch(&transport, &id) {
        Err(failure) if failure.session_gone() => None,
        _ => Some(idle / 3),
    })
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{answer, one_shot};

    const RENEWED: &str = "HTTP/1.1 200 OK\r\n\r\n{\"renewed\":true}";
    const BACKUPS_ONLY: &str = "HTTP/1.1 400 Bad Request\r\n\r\n\
        {\"type\":\"error\",\"code\":\"bad_request\",\"message\":\"only backup sessions can be renewed\"}";
    const NO_ROUTE: &str = "HTTP/1.1 404 Not Found\r\n\r\n\
        {\"type\":\"error\",\"code\":\"not_found\",\"message\":\"no such endpoint\"}";
    const GONE: &str = "HTTP/1.1 404 Not Found\r\n\r\n\
        {\"type\":\"error\",\"code\":\"no_such_session\",\"message\":\"gone\"}";
    const BUSY: &str = "HTTP/1.1 409 Conflict\r\n\r\n\
        {\"type\":\"error\",\"code\":\"session_busy\",\"message\":\"busy\"}";
    const ANSWERED: &str = "HTTP/1.1 200 OK\r\n\r\n";

    #[test]
    fn a_renew_tells_an_older_server_from_a_session_that_is_gone() {
        let (t, server) = one_shot(RENEWED);
        assert_eq!(session_renew(&t, "abc"), Ok(true));
        assert_eq!(server.join().unwrap(), "POST /sql/sessions/abc/renew HTTP/1.1");
        for older in [BACKUPS_ONLY, NO_ROUTE] {
            let (t, server) = one_shot(older);
            assert_eq!(session_renew(&t, "abc"), Ok(false));
            server.join().unwrap();
        }
        let (t, server) = one_shot(GONE);
        assert!(session_renew(&t, "abc").unwrap_err().session_gone());
        server.join().unwrap();
    }

    #[test]
    fn a_touch_falls_back_to_a_statement_once_per_server_that_renews_only_backups() {
        // The first touch asks for a renew, is told the server renews only
        // backups, and runs `SELECT 1` on the session instead.
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let t = Transport::Tcp(listener.local_addr().unwrap().to_string());
        let server = std::thread::spawn(move || {
            [BACKUPS_ONLY, ANSWERED, BUSY, GONE].map(|reply| answer(&mut listener.accept().unwrap().0, reply))
        });
        assert_eq!(session_touch(&t, "abc"), Ok(()));
        // From then on the server is asked with the statement alone: one
        // busy with another statement is alive, and a gone one is gone.
        assert_eq!(session_touch(&t, "abc"), Ok(()));
        assert!(session_touch(&t, "abc").unwrap_err().session_gone());
        assert_eq!(
            server.join().unwrap(),
            ["POST /sql/sessions/abc/renew HTTP/1.1", "POST /sql HTTP/1.1", "POST /sql HTTP/1.1", "POST /sql HTTP/1.1"]
        );
    }
}
