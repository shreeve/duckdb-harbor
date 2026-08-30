//! `GET /catalog` — the whole schema as one authenticated document.
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
    // Identity comes from /info; these stay optional so catalog decoding
    // never breaks if the server stops sending them.
    #[serde(default)]
    pub harbor_version: String,
    #[serde(default)]
    pub duckdb_version: String,
    #[serde(default)]
    pub tables: Vec<Table>,
    #[serde(default)]
    pub sequences: Vec<Sequence>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Table {
    pub name: String,
    pub schema: String,
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
    let r = request(
        &conn.transport,
        &wire::endpoint::CATALOG,
        conn.token.as_deref(),
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
            "tables": [
                {"name": "events", "schema": "main",
                 "columns": [{"name": "id", "type": "INTEGER", "notNull": true, "default": "nextval('id')", "primary": true}],
                 "primaryKey": ["id"], "uniqueConstraints": [], "indexes": [], "foreignKeys": []},
                {"name": "zeta", "schema": "audit", "columns": [], "primaryKey": []}
            ],
            "sequences": [{"name": "id", "start": 1}],
            "viewsSomeday": []
        }"#;
        let c: Catalog = serde_json::from_str(doc).unwrap();
        assert_eq!(c.schemas(), vec!["main", "audit"]);
        assert_eq!(c.tables_in("main")[0].columns[0].duck_type, "INTEGER");
        assert!(c.tables_in("main")[0].columns[0].primary);
        assert_eq!(c.sequences[0].name, "id");
    }
}
