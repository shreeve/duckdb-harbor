//! The Harbor wire contract — protocol 1.
//!
//! Everything a client and server must agree on lives here: endpoint paths,
//! request bodies, the NDJSON envelope, and the error codes. Codes are the
//! client interface (clients classify on `code`, never on HTTP status);
//! renaming one is a breaking change. Protocol 1 is what harbor v0.9.1
//! already speaks — fleet-era additions are new endpoints and optional
//! fields, never changes to existing shapes.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;

/// Content types. NDJSON is the default `/sql` response; `application/json`
/// selects the one-shot document (server-capped).
pub const CONTENT_NDJSON: &str = "application/x-ndjson";
pub const CONTENT_JSON: &str = "application/json";

pub mod endpoint {
    use std::borrow::Cow;

    /// A route is a method *and* a path. Harbor dispatches on the pair, so a
    /// path published without its verb is only half the contract — and the
    /// half that is missing fails quietly: `GET /sql` and `POST /ready` are
    /// 404s, not 405s, so a client that guesses wrong cannot tell a mistaken
    /// verb from an endpoint that never existed.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Route {
        pub method: &'static str,
        /// Borrowed for the fixed routes; owned by the two builders, whose
        /// paths carry an id.
        pub path: Cow<'static, str>,
    }

    impl Route {
        const fn fixed(method: &'static str, path: &'static str) -> Self {
            Self { method, path: Cow::Borrowed(path) }
        }

        fn built(method: &'static str, path: String) -> Self {
            Self { method, path: Cow::Owned(path) }
        }
    }

    impl std::fmt::Display for Route {
        /// `POST /sql` — the spelling harbor's logs and this crate's docs use.
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{} {}", self.method, self.path)
        }
    }

    /// POST — run exactly one statement. NDJSON unless the caller asks for
    /// `application/json`.
    pub const SQL: Route = Route::fixed("POST", "/sql");
    /// POST to the collection — open a session: a pinned connection that
    /// holds a transaction across requests. Release it with [`session`].
    /// (Servers also accept the legacy `/sql/sessions/new` spelling.)
    pub const SESSIONS_CREATE: Route = Route::fixed("POST", "/sql/sessions");
    /// The pre-0.22.1 spelling of [`SESSIONS_CREATE`]. Not in [`FIXED`]
    /// (new servers advertise the canonical route); clients keep it only
    /// to fall back when an older harbor answers 404 — joining older
    /// servers is a feature.
    pub const SESSIONS_CREATE_LEGACY: Route = Route::fixed("POST", "/sql/sessions/new");
    /// GET the collection — the open sessions. (Legacy spelling: `/sessions`.)
    pub const SESSIONS: Route = Route::fixed("GET", "/sql/sessions");
    /// GET — the whole schema as one document, cheaper and more complete than
    /// walking `duckdb_*()` a table at a time.
    pub const CATALOG: Route = Route::fixed("GET", "/catalog");
    /// POST, not DELETE — shutdown is an action, and DELETE names a resource
    /// to remove that doesn't exist ("delete the shutdown"). Fleet managers
    /// use this on platforms without Unix signals; the response is sent
    /// before draining. (Servers also accept the legacy DELETE.)
    pub const SHUTDOWN: Route = Route::fixed("POST", "/shutdown");
    /// GET — the only unauthenticated route, by design.
    pub const READY: Route = Route::fixed("GET", "/ready");
    /// GET — berth identity (auth required: it names paths and pids).
    pub const INFO: Route = Route::fixed("GET", "/info");

    /// Every fixed route, so a client (or a test on harbor's side) can walk
    /// the contract instead of transcribing it. The two builders below are
    /// not here: their paths carry an id, so there is nothing to enumerate.
    pub const FIXED: &[Route] =
        &[SQL, SESSIONS_CREATE, SESSIONS, CATALOG, SHUTDOWN, READY, INFO];

    /// DELETE — release the session `id` holds, rolling back any open
    /// transaction. A builder, since the id rides in the path.
    pub fn session(id: &str) -> Route {
        Route::built("DELETE", format!("/sql/sessions/{id}"))
    }

    /// DELETE — cancel the in-flight statement the caller tagged `id`. Racing
    /// a query that already finished is a no-op, not an error.
    pub fn query(id: &str) -> Route {
        Route::built("DELETE", format!("/sql/queries/{id}"))
    }

    /// GET — the catalog's lite style (harbor 0.17+): what exists and how
    /// big — tables with their schema and `estimatedRows`, the database's
    /// sizes — without how it is built (no columns, keys, indexes, DDL, or
    /// sequences). An older harbor ignores the parameter and answers the
    /// full document, so a client degrades to correct-but-heavier, never
    /// to an error.
    pub fn catalog_lite() -> Route {
        Route::built("GET", "/catalog?style=lite".to_string())
    }
}

/// Error codes in the wild. An open set — clients must pass unknown codes
/// through — but these names are frozen.
pub mod code {
    pub const BAD_REQUEST: &str = "bad_request";
    pub const SQL_ERROR: &str = "sql_error";
    pub const UNAUTHORIZED: &str = "unauthorized";
    pub const NOT_FOUND: &str = "not_found";
    pub const BODY_TOO_LARGE: &str = "body_too_large";
    pub const RESPONSE_TOO_LARGE: &str = "response_too_large";
    pub const CANCELLED: &str = "cancelled";
    pub const UNSUPPORTED_TYPE: &str = "unsupported_type";
    pub const NO_SUCH_SESSION: &str = "no_such_session";
    pub const SESSION_BUSY: &str = "session_busy";
    pub const QUERY_ID_IN_USE: &str = "query_id_in_use";
    pub const NO_LEASE_CONNECTIONS: &str = "no_lease_connections";
    pub const NO_LEASE_AVAILABLE: &str = "no_lease_available";
    pub const UNAVAILABLE: &str = "unavailable";
    pub const UNREADY: &str = "unready";
    pub const INTERNAL: &str = "internal";
}

/// POST /sql request body. `sql` must hold exactly one statement.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SqlRequest {
    pub sql: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Vec<Value>>,
    /// Route to a lease's pinned connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Client-chosen (≤128 chars); registers the cancellation handle before
    /// execution so DELETE /sql/queries/<id> can never race the start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_id: Option<String>,
    /// Absent = deployment default; 0 = explicitly unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// POST /sql/sessions request body (may be empty / absent).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionNewRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionNewResponse {
    pub session_id: String,
    pub ttl_ms: u64,
    pub idle_ttl_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ReleasedResponse {
    pub released: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelling: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CancelledResponse {
    pub cancelled: bool,
}

/// One column of a result schema, exactly as `emit_column_schema` writes it.
/// `name` is absent on nested positions (list children, map key/value);
/// `lossless` is always present. The per-type extras are optional and
/// mutually exclusive by construction.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Column {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub duckdb_type: String,
    pub lossless: bool,
    /// DECIMAL
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decimal: Option<DecimalMeta>,
    /// LIST and ARRAY element type
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child: Option<Box<Column>>,
    /// ARRAY only
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub array_length: Option<u64>,
    /// STRUCT
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields: Option<Vec<Column>>,
    /// MAP (values arrive as pairs; `encoding` says "pairs")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_type: Option<Box<Column>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_type: Option<Box<Column>>,
    /// UNION
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub members: Option<Vec<Column>>,
    /// ENUM
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<String>>,
    /// "pairs" | "varchar-cast"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct DecimalMeta {
    pub width: u8,
    pub scale: u8,
}

/// One line of the NDJSON stream: `schema`, then `row`*, then exactly one of
/// `end` or `error`. The same `error` shape is also the body of every non-2xx
/// HTTP response, so one decoder handles both faces.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Event {
    Schema {
        columns: Vec<Column>,
    },
    Row {
        values: Vec<Value>,
    },
    End {
        #[serde(rename = "rowCount")]
        row_count: u64,
        #[serde(rename = "timeMs")]
        time_ms: u64,
    },
    Error {
        code: String,
        message: String,
    },
}

impl Event {
    /// Parse one NDJSON line (or an HTTP error body).
    pub fn parse(line: &str) -> Result<Event, serde_json::Error> {
        serde_json::from_str(line)
    }
}

/// One-shot document (`Accept: application/json`), success shape. Failures
/// are the `Event::Error` shape with a real HTTP status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OneShotResponse {
    pub ok: bool,
    pub columns: Vec<Column>,
    pub data: Vec<Vec<Value>>,
    pub row_count: u64,
    pub time_ms: u64,
}

/// GET /info — fleet-era berth identity. Absent (404 `not_found`) on
/// pre-fleet servers; that absence is the version probe.
///
/// Lenient on read (`default`): a client usually wants one field of this,
/// and a missing sibling must not turn the whole document unreadable.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct InfoResponse {
    pub protocol_version: u32,
    pub name: String,
    pub harbor_version: String,
    pub duckdb_version: String,
    pub database: String,
    pub databases: Vec<String>,
    pub pid: u32,
    pub uptime_ms: u64,
    /// True when this server self-retires once its last client leaves (spawned
    /// with `HARBOR_EPHEMERAL` / a `detach start`). A client restarting it — to
    /// upgrade the binary, say — reads this to bring it back in the same mode.
    pub ephemeral: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn envelope_lines_round_trip() {
        // Canned lines exactly as the v0.9.1 emitter writes them.
        let schema = r#"{"type":"schema","columns":[{"name":"id","duckdbType":"BIGINT","lossless":true}]}"#;
        match Event::parse(schema).unwrap() {
            Event::Schema { columns } => {
                assert_eq!(columns[0].name.as_deref(), Some("id"));
                assert_eq!(columns[0].duckdb_type, "BIGINT");
                assert!(columns[0].lossless);
            }
            other => panic!("wrong event: {other:?}"),
        }

        let row = r#"{"type":"row","values":[0,"row0",null]}"#;
        assert_eq!(
            Event::parse(row).unwrap(),
            Event::Row { values: vec![json!(0), json!("row0"), Value::Null] }
        );

        let end = r#"{"type":"end","rowCount":3,"timeMs":2}"#;
        assert_eq!(Event::parse(end).unwrap(), Event::End { row_count: 3, time_ms: 2 });
    }

    #[test]
    fn error_shape_serves_both_faces() {
        // In-stream terminal event and HTTP error body are the same shape.
        let e = r#"{"type":"error","code":"unauthorized","message":"missing or invalid bearer token"}"#;
        match Event::parse(e).unwrap() {
            Event::Error { code, message } => {
                assert_eq!(code, code::UNAUTHORIZED);
                assert!(message.contains("bearer"));
            }
            other => panic!("wrong event: {other:?}"),
        }
    }

    #[test]
    fn typed_columns_decode() {
        let decimal = r#"{"name":"d","duckdbType":"DECIMAL(12,3)","lossless":true,"decimal":{"width":12,"scale":3}}"#;
        let c: Column = serde_json::from_str(decimal).unwrap();
        assert_eq!(c.decimal, Some(DecimalMeta { width: 12, scale: 3 }));

        let list = r#"{"name":"l","duckdbType":"BIGINT[]","lossless":true,"child":{"duckdbType":"BIGINT","lossless":true}}"#;
        let c: Column = serde_json::from_str(list).unwrap();
        assert_eq!(c.child.unwrap().duckdb_type, "BIGINT");

        let map = r#"{"name":"m","duckdbType":"MAP(TEXT, BIGINT)","lossless":true,"keyType":{"duckdbType":"VARCHAR","lossless":true},"valueType":{"duckdbType":"BIGINT","lossless":true},"encoding":"pairs"}"#;
        let c: Column = serde_json::from_str(map).unwrap();
        assert_eq!(c.encoding.as_deref(), Some("pairs"));

        let lossy = r#"{"name":"g","duckdbType":"GEOMETRY","lossless":false,"encoding":"varchar-cast"}"#;
        let c: Column = serde_json::from_str(lossy).unwrap();
        assert!(!c.lossless);
    }

    #[test]
    fn requests_serialize_sparse() {
        // Optional fields stay off the wire — matches what existing clients send.
        let req = SqlRequest { sql: "SELECT 1".into(), ..Default::default() };
        assert_eq!(serde_json::to_string(&req).unwrap(), r#"{"sql":"SELECT 1"}"#);

        let req = SqlRequest {
            sql: "SELECT ?".into(),
            params: Some(vec![json!(1)]),
            session_id: Some("s1".into()),
            query_id: Some("q1".into()),
            timeout_ms: Some(0),
        };
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            r#"{"sql":"SELECT ?","params":[1],"sessionId":"s1","queryId":"q1","timeoutMs":0}"#
        );
    }
}
