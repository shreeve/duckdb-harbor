//! `POST /sql` — run one statement and decode its NDJSON stream.
//!
//! The wire contract (wire::Event) is: one `schema` line, then `row` lines,
//! then exactly one `end` or `error`. The same `error` shape is also the
//! body of every non-2xx response, so one loop decodes both faces.

use crate::fleet::Conn;
use crate::http;
use std::io::BufRead as _;
use std::time::Duration;
use wire::{endpoint, Event, SqlRequest};

pub use http::Failure;

/// One statement's full result page, in server order.
#[derive(Debug)]
pub struct QueryResult {
    pub columns: Vec<wire::Column>,
    pub rows: Vec<Vec<serde_json::Value>>,
    pub row_count: u64,
    pub time_ms: u64,
}

pub fn query(conn: &Conn, sql: &str) -> Result<QueryResult, String> {
    exec(conn, sql, None, None)
}

/// One statement with everything the wire offers: bound parameters (never
/// string-assembled values) and an optional session, whose pinned
/// connection is what lets a transaction outlive one request.
pub fn exec(
    conn: &Conn,
    sql: &str,
    params: Option<Vec<serde_json::Value>>,
    session_id: Option<&str>,
) -> Result<QueryResult, String> {
    exec_checked(conn, sql, params, session_id).map_err(|e| e.to_string())
}

/// [`exec`], with a failure that says whether Harbor answered.
pub fn exec_checked(
    conn: &Conn,
    sql: &str,
    params: Option<Vec<serde_json::Value>>,
    session_id: Option<&str>,
) -> Result<QueryResult, Failure> {
    exec_within(conn, sql, params, session_id, Duration::from_secs(120))
}

/// [`exec_checked`], waiting at most `patience` for each read of the answer.
/// For a statement that answers at once or not at all, such as a keepalive.
pub fn exec_within(
    conn: &Conn,
    sql: &str,
    params: Option<Vec<serde_json::Value>>,
    session_id: Option<&str>,
    patience: Duration,
) -> Result<QueryResult, Failure> {
    let body = serde_json::to_string(&SqlRequest {
        sql: sql.to_string(),
        params,
        session_id: session_id.map(str::to_string),
        ..Default::default()
    })
    .map_err(|e| Failure::Unsent(e.to_string()))?;
    // A tunnel that has died takes its route with it: nothing can be sent.
    let transport = conn.transport().map_err(Failure::Unsent)?;
    let resp = http::request(transport, &endpoint::SQL, Some(&body), Some(patience)).map_err(Failure::from)?;

    // Status first: a non-2xx or a proxy's HTML body must answer as itself, not
    // as "bad wire line" from trying to decode it as NDJSON.
    let status = resp.status;
    if !(200..300).contains(&status) {
        return Err(Failure::of(status, &resp.body_string().unwrap_or_default()));
    }
    decode(resp.body.lines())
}

/// Read a result from its NDJSON lines. The stream is complete only when
/// its `end` event arrives: one that stops short of it, because the server
/// died or the connection dropped mid-answer, is no verdict, however many
/// rows came first. Nor is one out of the wire's order: a row before the
/// schema, a second schema, or anything after the end.
fn decode(lines: impl Iterator<Item = std::io::Result<String>>) -> Result<QueryResult, Failure> {
    let mut columns = None;
    let mut rows = Vec::new();
    let mut end = None;
    for line in lines {
        let line = line.map_err(|e| Failure::Unanswered(format!("stream: {e}")))?;
        if line.trim().is_empty() {
            continue;
        }
        let event = Event::parse(&line).map_err(|e| Failure::Unanswered(format!("bad wire line: {e}")))?;
        match event {
            _ if end.is_some() => return Err(Failure::Unanswered("bad wire line: the answer went on after its end".into())),
            Event::Schema { columns: c } if columns.is_none() => columns = Some(c),
            Event::Row { values } if columns.is_some() => rows.push(values),
            Event::End { row_count, time_ms } => end = Some((row_count, time_ms)),
            Event::Error { code, message } => return Err(Failure::Refused { code, message }),
            Event::Schema { .. } => return Err(Failure::Unanswered("bad wire line: a second schema".into())),
            Event::Row { .. } => return Err(Failure::Unanswered("bad wire line: a row before the schema".into())),
        }
    }
    let columns = columns.unwrap_or_default();
    let Some((row_count, time_ms)) = end else {
        return Err(Failure::Unanswered("stream: the answer ended before it was complete".to_string()));
    };
    Ok(QueryResult { columns, rows, row_count, time_ms })
}

/// A session as Harbor granted it.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    /// How long the session lives from its opening, whatever runs on it.
    pub ttl: Duration,
    /// How long it may sit between statements before Harbor reclaims it.
    /// Zero means it has no idle timeout.
    pub idle: Duration,
}

/// Open a session: a pinned connection that holds a transaction across
/// requests. Release it with [`session_release`] — which rolls back
/// anything uncommitted, so an abandoned session can never half-commit.
pub fn session_new(conn: &Conn) -> Result<String, String> {
    session_open(conn).map(|session| session.id)
}

/// [`session_new`], with the lifetime Harbor granted.
pub fn session_open(conn: &Conn) -> Result<Session, String> {
    http::session_open(conn.transport()?, &Default::default())
        .map(|r| Session {
            id: r.session_id,
            ttl: Duration::from_millis(r.ttl_ms),
            idle: Duration::from_millis(r.idle_ttl_ms),
        })
        .map_err(|e| format!("session: {e}"))
}

/// Renew a session without running anything on it: its idle clock starts
/// again, and its ceiling stays. `Ok(false)` is a Harbor that renews only
/// backup sessions, where a statement is what keeps a session alive.
pub fn session_renew(conn: &Conn, session_id: &str) -> Result<bool, Failure> {
    http::session_renew(conn.transport().map_err(Failure::Unsent)?, session_id)
}

/// Release a session's pinned connection, rolling back any open
/// transaction. Best-effort by design: the server's TTL reaps what a
/// dropped connection leaves behind.
pub fn session_release(conn: &Conn, session_id: &str) {
    let _ = release(conn, session_id);
}

/// What became of a session that was asked to end.
#[derive(Debug, Clone, PartialEq)]
pub enum Ended {
    /// It is over: whatever was open on it is rolled back, and whatever it
    /// committed is committed. What the database shows is its outcome.
    Settled,
    /// A statement was still running on it. Harbor is cancelling that and
    /// ends the session when it returns, which had not happened yet when
    /// the wait ran out.
    StillRunning,
    /// Harbor could not be asked. The reason.
    Unknown(String),
}

/// End a session and wait, up to `patience`, until it is over. Releasing an
/// idle session rolls it back before Harbor answers. One busy with a
/// statement, as a session is whose `COMMIT` outlived the wait for its
/// answer, is cancelled and ends when the statement returns, so it is
/// watched in Harbor's list of sessions until it leaves.
pub fn session_end(conn: &Conn, session_id: &str, patience: Duration) -> Ended {
    let cancelling = match release(conn, session_id) {
        Ok(answer) => answer.cancelling == Some(true),
        Err(why) => return Ended::Unknown(why),
    };
    if !cancelling {
        return Ended::Settled;
    }
    let deadline = std::time::Instant::now() + patience;
    loop {
        match session_listed(conn, session_id) {
            Ok(false) => return Ended::Settled,
            Ok(true) if std::time::Instant::now() >= deadline => return Ended::StillRunning,
            Ok(true) => std::thread::sleep(Duration::from_millis(100)),
            Err(why) => return Ended::Unknown(why),
        }
    }
}

fn release(conn: &Conn, session_id: &str) -> Result<wire::ReleasedResponse, String> {
    http::session_release(conn.transport()?, session_id).map_err(|e| format!("release: {e}"))
}

/// Whether Harbor still lists the session among those it holds.
fn session_listed(conn: &Conn, session_id: &str) -> Result<bool, String> {
    let resp = http::request(conn.transport()?, &endpoint::SESSIONS, None, Some(Duration::from_secs(5)))
        .map_err(|e| format!("sessions: {e}"))?;
    let status = resp.status;
    let body = resp.body_string().map_err(|e| format!("sessions: {e}"))?;
    if status != 200 {
        return Err(format!("sessions: HTTP {status}"));
    }
    listed(&body, session_id)
}

/// Read Harbor's list of sessions for one id.
fn listed(body: &str, session_id: &str) -> Result<bool, String> {
    let list: serde_json::Value =
        serde_json::from_str(body.trim()).map_err(|e| format!("sessions: {e}"))?;
    let sessions = list
        .get("sessions")
        .and_then(|s| s.as_array())
        .ok_or_else(|| "sessions: no list in the answer".to_string())?;
    Ok(sessions.iter().any(|s| s.get("sessionId").and_then(|id| id.as_str()) == Some(session_id)))
}

#[cfg(test)]
mod tests {
    use super::{decode, listed, Failure};

    #[test]
    fn a_session_is_listed_until_harbor_lets_it_go() {
        let body = r#"{"serving":true,"connections":{"total":10,"free":9,"live":1,"inflight":0,"balanced":true},"sessions":[{"sessionId":"abc","slot":1,"ageMs":5,"idleMs":1,"expiresInMs":299995,"statements":2,"inTransaction":true,"busy":true,"renewable":false}]}"#;
        assert_eq!(listed(body, "abc"), Ok(true));
        assert_eq!(listed(body, "abd"), Ok(false));
        assert_eq!(listed(r#"{"serving":true,"sessions":[]}"#, "abc"), Ok(false));
        // An answer that is not the list says nothing, and is not "gone".
        assert!(listed(r#"{"serving":false}"#, "abc").is_err());
        assert!(listed("<html>", "abc").is_err());
    }

    fn lines(text: &str) -> impl Iterator<Item = std::io::Result<String>> + '_ {
        text.lines().map(|line| Ok(line.to_string()))
    }

    const SCHEMA: &str = r#"{"type":"schema","columns":[{"name":"v","duckdbType":"INTEGER","lossless":true}]}"#;

    #[test]
    fn a_result_is_complete_only_at_its_end_event() {
        let whole = format!("{SCHEMA}\n{{\"type\":\"row\",\"values\":[1]}}\n\n{{\"type\":\"end\",\"rowCount\":1,\"timeMs\":2}}\n");
        let result = decode(lines(&whole)).unwrap();
        assert_eq!((result.rows.len(), result.row_count, result.time_ms), (1, 1, 2));
        assert_eq!(result.columns[0].name.as_deref(), Some("v"));

        // Cut off after a row, after the schema, or before anything: no
        // verdict, so not a success with fewer rows.
        for cut in [
            format!("{SCHEMA}\n{{\"type\":\"row\",\"values\":[1]}}\n"),
            format!("{SCHEMA}\n"),
            String::new(),
        ] {
            let failure = decode(lines(&cut)).unwrap_err();
            assert!(matches!(&failure, Failure::Unanswered(why) if why.contains("before it was complete")), "{cut:?}: {failure:?}");
        }

        // An error event is Harbor's verdict, wherever it comes.
        let refused = format!("{SCHEMA}\n{{\"type\":\"error\",\"code\":\"sql_error\",\"message\":\"boom\"}}\n");
        assert_eq!(
            decode(lines(&refused)).unwrap_err(),
            Failure::Refused { code: "sql_error".into(), message: "boom".into() }
        );
        // A read that fails mid-stream, and a line that is not the wire's.
        let broken = [Ok(SCHEMA.to_string()), Err(std::io::Error::other("connection closed mid-chunk"))];
        assert!(matches!(decode(broken.into_iter()).unwrap_err(), Failure::Unanswered(why) if why.starts_with("stream: ")));
        assert!(matches!(decode(lines("<html>")).unwrap_err(), Failure::Unanswered(why) if why.starts_with("bad wire line")));
    }

    #[test]
    fn a_result_out_of_the_wires_order_is_no_verdict() {
        const ROW: &str = r#"{"type":"row","values":[1]}"#;
        const END: &str = r#"{"type":"end","rowCount":1,"timeMs":2}"#;
        for (text, why) in [
            (format!("{ROW}\n{SCHEMA}\n{END}\n"), "a row before the schema"),
            (format!("{SCHEMA}\n{SCHEMA}\n{END}\n"), "a second schema"),
            (format!("{SCHEMA}\n{END}\n{ROW}\n"), "after its end"),
            (format!("{SCHEMA}\n{END}\n{END}\n"), "after its end"),
        ] {
            let failure = decode(lines(&text)).unwrap_err();
            assert!(matches!(&failure, Failure::Unanswered(m) if m.ends_with(why)), "{text:?}: {failure:?}");
        }
        // An end alone is an answer with no columns.
        assert_eq!(decode(lines(&format!("{END}\n"))).unwrap().columns.len(), 0);
    }
}
