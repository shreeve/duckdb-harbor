//! `GET /catalog` — the whole schema as one document.
//!
//! Harbor curates what the engine's catalog functions expose, so version
//! differences between DuckDB releases vanish before they reach a client.
//! The shapes here decode what the server sends today (tables and
//! sequences) and ignore sections it may grow later, so a newer Harbor
//! never breaks an older DuckTable.

use crate::fleet::Conn;
use crate::http::request;
use serde::Deserialize;
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    // The response also carries harbor/duckdb versions; identity comes
    // from /info, so they are not modeled here.
    #[serde(default)]
    pub tables: Vec<Table>,
    #[serde(default)]
    pub sequences: Vec<Sequence>,
    /// Exact bytes of the served file, statted by the server (harbor
    /// 0.18+); None from an older Harbor or a berth serving no file.
    #[serde(default)]
    pub database_size_bytes: Option<u64>,
    /// Exact bytes of the WAL beside it — 0 after a checkpoint, which is
    /// an answer, not an absence.
    #[serde(default)]
    pub wal_size_bytes: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Table {
    pub name: String,
    pub schema: String,
    /// The engine's cardinality estimate (harbor 0.18+) — a sidebar
    /// figure, not a COUNT(*).
    #[serde(default)]
    pub estimated_rows: Option<u64>,
    /// The engine's own CREATE TABLE rendering (harbor 0.18+).
    #[serde(default)]
    pub ddl: Option<String>,
    #[serde(default)]
    pub columns: Vec<Column>,
    #[serde(default)]
    pub primary_key: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Column {
    pub name: String,
    #[serde(rename = "type")]
    pub duck_type: String,
    #[serde(default)]
    pub not_null: bool,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub primary: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sequence {
    pub name: String,
}

impl Catalog {
    /// Schemas in display order, `main` first the way DuckDB presents it.
    pub fn schemas(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.tables.iter().map(|t| t.schema.as_str()).collect();
        v.sort_unstable();
        v.dedup();
        if let Some(pos) = v.iter().position(|s| *s == "main") {
            let main = v.remove(pos);
            v.insert(0, main);
        }
        v
    }

    pub fn tables_in(&self, schema: &str) -> Vec<&Table> {
        let mut v: Vec<&Table> = self.tables.iter().filter(|t| t.schema == schema).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }
}

pub fn catalog(conn: &Conn) -> Result<Catalog, String> {
    fetch(conn, &wire::endpoint::CATALOG)
}

/// The catalog's lite style (harbor 0.18+): the versions, the sizes, and
/// each table as name, schema, and `estimatedRows` — enough to draw a
/// database list without paying for columns, DDL, or sequences. An older
/// harbor ignores the parameter and answers the full document, so this
/// degrades to correct-but-heavier, never to an error.
pub fn catalog_lite(conn: &Conn) -> Result<Catalog, String> {
    fetch(conn, &wire::endpoint::catalog_lite())
}

fn fetch(conn: &Conn, route: &wire::endpoint::Route) -> Result<Catalog, String> {
    let r = request(
        conn.transport()?,
        route,
        None,
        Some(Duration::from_secs(15)),
    )
    .map_err(|e| e.to_string())?;
    let body = r.body_string().map_err(|e| e.to_string())?;
    if let Ok(c) = serde_json::from_str::<Catalog>(&body) {
        return Ok(c);
    }
    match wire::Event::parse(body.trim()) {
        Ok(wire::Event::Error { code, message }) => Err(format!("{code}: {message}")),
        _ => Err(format!(
            "unexpected /catalog response: {}",
            body.chars().take(120).collect::<String>()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_the_live_shape_and_ignores_growth() {
        let doc = r#"{
            "harborVersion": "0.15.0",
            "duckdbVersion": "v2.0.0",
            "databaseSizeBytes": 1310720,
            "walSizeBytes": 0,
            "tables": [
                {"name": "events", "schema": "main", "estimatedRows": 42,
                 "columns": [{"name": "id", "type": "INTEGER", "notNull": true, "default": "nextval('id')", "primary": true}],
                 "primaryKey": ["id"], "uniqueConstraints": [], "indexes": [], "foreignKeys": [],
                 "ddl": "CREATE TABLE events(id INTEGER PRIMARY KEY DEFAULT(nextval('id')));"},
                {"name": "zeta", "schema": "audit", "columns": [], "primaryKey": []}
            ],
            "sequences": [{"name": "id", "start": 1}],
            "viewsSomeday": []
        }"#;
        let c: Catalog = serde_json::from_str(doc).unwrap();
        assert_eq!(c.schemas(), vec!["main", "audit"]);
        assert_eq!(c.tables_in("main")[0].columns[0].duck_type, "INTEGER");
        assert!(c.tables_in("main")[0].columns[0].primary);
        assert_eq!(c.tables_in("main")[0].estimated_rows, Some(42));
        assert!(c.tables_in("main")[0].ddl.as_deref().unwrap().starts_with("CREATE TABLE"));
        // The audit table came from an older Harbor with no enrichment:
        // every new field is an absence, never an error.
        assert_eq!(c.tables_in("audit")[0].estimated_rows, None);
        assert_eq!(c.database_size_bytes, Some(1310720));
        assert_eq!(c.wal_size_bytes, Some(0));
        assert_eq!(c.sequences[0].name, "id");
    }
}
