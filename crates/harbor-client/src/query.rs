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

/// One statement's full result page, in server order.
pub struct QueryResult {
    pub columns: Vec<wire::Column>,
    pub rows: Vec<Vec<serde_json::Value>>,
    pub row_count: u64,
    pub time_ms: u64,
}

pub fn query(conn: &Conn, sql: &str) -> Result<QueryResult, String> {
    let body = serde_json::to_string(&SqlRequest { sql: sql.to_string(), ..Default::default() })
        .map_err(|e| e.to_string())?;
    let resp = http::request(
        &conn.transport,
        &endpoint::SQL,
        conn.token.as_deref(),
        Some(&body),
        Some(Duration::from_secs(120)),
    )
    .map_err(|e| format!("query: {e}"))?;

    // Status first: a 401 or a proxy's HTML body must answer as itself, not
    // as "bad wire line" from trying to decode it as NDJSON.
    let status = resp.status;
    if !(200..300).contains(&status) {
        let body = resp.body_string().unwrap_or_default();
        return Err(match Event::parse(body.trim()) {
            Ok(Event::Error { code, message }) => format!("{code}: {message}"),
            _ => format!("HTTP {status}"),
        });
    }
    let mut columns = Vec::new();
    let mut rows = Vec::new();
    let mut row_count = 0;
    let mut time_ms = 0;
    for line in resp.body.lines() {
        let line = line.map_err(|e| format!("stream: {e}"))?;
        if line.trim().is_empty() {
            continue;
        }
        match Event::parse(&line).map_err(|e| format!("bad wire line: {e}"))? {
            Event::Schema { columns: c } => columns = c,
            Event::Row { values } => rows.push(values),
            Event::End { row_count: n, time_ms: t } => {
                row_count = n;
                time_ms = t;
            }
            Event::Error { code, message } => return Err(format!("{code}: {message}")),
        }
    }
    Ok(QueryResult { columns, rows, row_count, time_ms })
}
