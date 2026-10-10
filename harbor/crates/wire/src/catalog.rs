//! `GET /catalog`: the whole schema as one document.
//!
//! The server writes these types and a client reads them, so the two cannot
//! disagree on a name. Field order is the document's order, and every field
//! the full style writes is always there, `null` or `[]` when empty. The lite
//! style ([`Inventory`]) is the same document at lower fidelity: a field it
//! omits is absent, never differently shaped. Reading is lenient: a field
//! missing, as everything the lite style omits is, reads as its default, and
//! one a later server adds is ignored.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The full style.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Catalog {
    pub harbor_version: String,
    pub duckdb_version: String,
    /// Exact bytes of the served file, statted by the server; None for a
    /// berth serving no file.
    pub database_size_bytes: Option<u64>,
    /// Exact bytes of the WAL beside it: 0 after a checkpoint, which is an
    /// answer, not an absence.
    pub wal_size_bytes: Option<u64>,
    /// By (schema, name).
    pub tables: Vec<Table>,
    /// By name.
    pub sequences: Vec<Sequence>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Table {
    pub name: String,
    pub schema: String,
    /// Exact COUNT(*); absent from the lite style.
    pub row_count: Option<u64>,
    /// In ordinal position.
    pub columns: Vec<Column>,
    pub primary_key: Vec<String>,
    /// Inline and table-level UNIQUE, by their column lists. The internal
    /// indexes behind them are not in `indexes`.
    pub unique_constraints: Vec<Unique>,
    /// What CREATE INDEX made, by name.
    pub indexes: Vec<Index>,
    /// By referenced table, then column lists.
    pub foreign_keys: Vec<ForeignKey>,
    /// The engine's own CREATE TABLE text, last: it runs long, and the fields
    /// a reader scans for stay up front.
    pub ddl: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Column {
    pub name: String,
    #[serde(rename = "type")]
    pub duck_type: String,
    pub not_null: bool,
    pub default: Option<String>,
    pub generated: bool,
    pub generation_expression: Option<String>,
    pub primary: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Unique {
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Index {
    pub name: String,
    pub columns: Vec<String>,
    /// Computed entries, kept apart from `columns` on purpose: a differ that
    /// joined one against `columns[].name` would be matching on a rendering.
    pub expressions: Vec<String>,
    pub unique: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ForeignKey {
    pub columns: Vec<String>,
    pub ref_table: String,
    /// The referencing table's own: DuckDB refuses a foreign key across
    /// schemas.
    pub ref_schema: String,
    pub ref_columns: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Sequence {
    pub name: String,
    /// Harbor's integer policy: a number within JSON's exact range, past it
    /// its decimal string.
    pub start: Value,
}

/// The lite style, as the server writes it: what exists, without counts or
/// how anything is built. A client reads it as a [`Catalog`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Inventory {
    pub harbor_version: String,
    pub duckdb_version: String,
    pub database_size_bytes: Option<u64>,
    pub wal_size_bytes: Option<u64>,
    pub tables: Vec<Named>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Named {
    pub name: String,
    pub schema: String,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_the_full_shape_and_ignores_growth() {
        let doc = r#"{
            "harborVersion": "0.15.0",
            "duckdbVersion": "v2.0.0",
            "databaseSizeBytes": 1310720,
            "walSizeBytes": 0,
            "tables": [
                {"name": "events", "schema": "main", "rowCount": 42,
                 "columns": [{"name": "id", "type": "INTEGER", "notNull": true, "default": "nextval('id')", "generated": false, "generationExpression": null, "primary": true},
                              {"name": "slug", "type": "VARCHAR", "notNull": false, "default": "lower(name)", "generated": true, "generationExpression": "lower(name)", "primary": false}],
                 "primaryKey": ["id"], "uniqueConstraints": [{"columns": ["slug"]}], "indexes": [],
                 "foreignKeys": [{"columns": ["id"], "refTable": "zeta", "refSchema": "main", "refColumns": ["id"]}],
                 "ddl": "CREATE TABLE events(id INTEGER PRIMARY KEY DEFAULT(nextval('id')));"},
                {"name": "zeta", "schema": "audit", "rowCount": 7, "columns": [], "primaryKey": []}
            ],
            "sequences": [{"name": "id", "start": 1}],
            "viewsSomeday": []
        }"#;
        let c: Catalog = serde_json::from_str(doc).unwrap();
        assert_eq!(c.schemas(), vec!["main", "audit"]);
        let events = c.tables_in("main")[0];
        assert_eq!(events.columns[0].duck_type, "INTEGER");
        assert!(events.columns[0].primary && !events.columns[0].generated);
        assert_eq!(events.columns[1].generation_expression.as_deref(), Some("lower(name)"));
        assert_eq!(events.row_count, Some(42));
        assert_eq!(events.unique_constraints[0].columns, ["slug"]);
        assert_eq!(events.foreign_keys[0].ref_table, "zeta");
        assert!(events.ddl.as_deref().unwrap().starts_with("CREATE TABLE"));
        assert_eq!((c.database_size_bytes, c.wal_size_bytes), (Some(1310720), Some(0)));
        assert_eq!(c.sequences[0].start, 1);
    }

    /// Written in the document's order, with nothing left out of the full
    /// style, and the lite style reads back as a catalog of its tables.
    #[test]
    fn the_server_writes_what_a_client_reads() {
        let table = Table { name: "t".into(), schema: "main".into(), row_count: Some(0), ..Default::default() };
        let full = Catalog { tables: vec![table], ..Default::default() };
        assert_eq!(
            serde_json::to_string(&full).unwrap(),
            r#"{"harborVersion":"","duckdbVersion":"","databaseSizeBytes":null,"walSizeBytes":null,"tables":[{"name":"t","schema":"main","rowCount":0,"columns":[],"primaryKey":[],"uniqueConstraints":[],"indexes":[],"foreignKeys":[],"ddl":null}],"sequences":[]}"#
        );
        let lite = Inventory {
            harbor_version: "1".into(),
            duckdb_version: "v2".into(),
            database_size_bytes: Some(1),
            wal_size_bytes: Some(0),
            tables: vec![Named { name: "t".into(), schema: "main".into() }],
        };
        let text = serde_json::to_string(&lite).unwrap();
        assert_eq!(
            text,
            r#"{"harborVersion":"1","duckdbVersion":"v2","databaseSizeBytes":1,"walSizeBytes":0,"tables":[{"name":"t","schema":"main"}]}"#
        );
        let read: Catalog = serde_json::from_str(&text).unwrap();
        assert_eq!((read.tables[0].row_count, read.tables[0].columns.len()), (None, 0));
    }
}
