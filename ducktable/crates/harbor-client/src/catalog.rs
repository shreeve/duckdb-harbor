//! `GET /catalog` — the whole schema as one document, in the types the
//! server writes it with (`wire::catalog`).
//!
//! Harbor curates what the engine's catalog functions expose, so version
//! differences between DuckDB releases vanish before they reach a client.

use crate::fleet::Conn;
use crate::http::request;
use std::time::Duration;

pub use wire::catalog::{Catalog, Column, Sequence, Table};

pub fn catalog(conn: &Conn) -> Result<Catalog, String> {
    fetch(conn, &wire::endpoint::CATALOG)
}

/// The catalog's lite style: versions, sizes, and table names/schemas — enough
/// to draw a database list without paying for counts, columns, DDL, or
/// sequences.
pub fn catalog_lite(conn: &Conn) -> Result<Catalog, String> {
    fetch(conn, &wire::endpoint::catalog_lite())
}

fn fetch(conn: &Conn, route: &wire::endpoint::Route) -> Result<Catalog, String> {
    let r = request(conn.transport()?, route, None, Some(Duration::from_secs(15)))
        .map_err(|e| e.to_string())?;
    let status = r.status;
    let body = r.body_string().map_err(|e| e.to_string())?;
    decode(status, &body)
}

/// Status first: every field of a catalog has a default, so an error body
/// would decode as an empty one, and a refresh that failed would replace the
/// schema on screen with nothing.
fn decode(status: u16, body: &str) -> Result<Catalog, String> {
    if status != 200 {
        return Err(match wire::Event::parse(body.trim()) {
            Ok(wire::Event::Error { code, message }) => format!("{code}: {message}"),
            _ => format!("HTTP {status}"),
        });
    }
    serde_json::from_str(body).map_err(|_| {
        format!("unexpected /catalog response: {}", body.chars().take(120).collect::<String>())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_answer_is_an_error_never_an_empty_catalog() {
        let error = r#"{"type":"error","code":"unavailable","message":"harbor is not serving"}"#;
        assert_eq!(decode(503, error).unwrap_err(), "unavailable: harbor is not serving");
        assert_eq!(decode(502, "<html>bad gateway</html>").unwrap_err(), "HTTP 502");
        let c = decode(200, r#"{"tables":[{"name":"events","schema":"main"}]}"#).unwrap();
        assert_eq!((c.tables[0].name.as_str(), c.tables[0].row_count), ("events", None));
    }
}
