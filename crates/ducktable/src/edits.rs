//! The staging layer (docs/EDITING.md): every change the user has made
//! and not yet committed, keyed by row identity — the primary-key
//! columns' original fetched values — never by grid position. The view
//! can sort, filter, and page freely; nothing here moves.
//!
//! This module is pure model: no GPUI, no HTTP. The grid projects it
//! onto the current page for rendering; commit turns it into
//! parameterized statements. Every mutation — including a discard — is
//! one entry on the undo stack, so nothing is ever more than one
//! keystroke from recovery.

use gpui::SharedString;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

/// One cell's staged change. Display text and bind value are both kept:
/// the text is what render shows and what auto-clean compares; the value
/// is what the statement binds (Null for NULL).
#[derive(Clone, Debug, PartialEq)]
pub struct CellEdit {
    /// The fetched display text this edit replaces (None = NULL).
    pub original: Option<SharedString>,
    /// The staged display text (None = NULL).
    pub text: Option<SharedString>,
    /// The value the UPDATE binds.
    pub value: Value,
}

/// One row's staged fate.
#[derive(Clone, Debug, PartialEq)]
pub enum RowChange {
    /// Schema column index -> staged cell.
    Update(BTreeMap<usize, CellEdit>),
    Delete,
}

/// One undo step: the row entry's state before and after a mutation.
/// Undo restores `prev`, redo restores `next` — one uniform shape for
/// edits, clears, deletes, and discards.
struct Op {
    key: String,
    identity: Vec<Value>,
    prev: Option<RowChange>,
    next: Option<RowChange>,
}

struct Entry {
    identity: Vec<Value>,
    change: RowChange,
}

/// The staged-change set for one table.
pub struct Edits {
    /// Quoted `"schema"."table"` the statements target.
    source: String,
    /// Primary-key column names, in key order.
    pk_cols: Vec<String>,
    /// All schema column names, in result order (for SET clauses).
    columns: Vec<String>,
    changes: HashMap<String, Entry>,
    undo: Vec<Op>,
    redo: Vec<Op>,
}

/// A row identity's map key: its canonical JSON. Values compare by
/// serialization, which is exactly the equality the wire speaks.
pub fn key_of(identity: &[Value]) -> String {
    serde_json::to_string(identity).unwrap_or_default()
}

impl Edits {
    pub fn new(source: String, pk_cols: Vec<String>, columns: Vec<String>) -> Self {
        Self {
            source,
            pk_cols,
            columns,
            changes: HashMap::new(),
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// (updates, deletes) — the verb-split the status line shows.
    pub fn counts(&self) -> (usize, usize) {
        let deletes = self
            .changes
            .values()
            .filter(|e| matches!(e.change, RowChange::Delete))
            .count();
        (self.changes.len() - deletes, deletes)
    }

    /// The staged display text for a cell, if any. `Some(None)` means
    /// staged NULL.
    #[cfg(test)]
    pub fn staged_text(&self, key: &str, col: usize) -> Option<Option<SharedString>> {
        match &self.changes.get(key)?.change {
            RowChange::Update(cells) => cells.get(&col).map(|c| c.text.clone()),
            RowChange::Delete => None,
        }
    }

    #[cfg(test)]
    pub fn is_deleted(&self, key: &str) -> bool {
        matches!(self.changes.get(key).map(|e| &e.change), Some(RowChange::Delete))
    }

    /// Every staged change, for the review popover: (key, identity,
    /// change), deterministically ordered.
    pub fn entries(&self) -> Vec<(&str, &[Value], &RowChange)> {
        let mut v: Vec<_> = self
            .changes
            .iter()
            .map(|(k, e)| (k.as_str(), e.identity.as_slice(), &e.change))
            .collect();
        v.sort_by(|a, b| a.0.cmp(b.0));
        v
    }

    pub fn column_name(&self, ix: usize) -> &str {
        self.columns.get(ix).map(String::as_str).unwrap_or("?")
    }

    fn apply(&mut self, op: Op) {
        match &op.next {
            Some(change) => {
                self.changes.insert(
                    op.key.clone(),
                    Entry { identity: op.identity.clone(), change: change.clone() },
                );
            }
            None => {
                self.changes.remove(&op.key);
            }
        }
        self.undo.push(op);
        self.redo.clear();
    }

    /// Stage one cell. Editing a value back to its original auto-cleans;
    /// editing a cell on a staged-deleted row first un-stages the delete
    /// (you cannot edit a ghost). One entry per cell, last wins.
    pub fn stage_cell(
        &mut self,
        identity: Vec<Value>,
        col: usize,
        original: Option<SharedString>,
        text: Option<SharedString>,
        value: Value,
    ) {
        let key = key_of(&identity);
        let prev = self.changes.get(&key).map(|e| e.change.clone());
        let mut cells = match &prev {
            Some(RowChange::Update(cells)) => cells.clone(),
            _ => BTreeMap::new(),
        };
        if text == original {
            cells.remove(&col);
        } else {
            cells.insert(col, CellEdit { original, text, value });
        }
        let next = (!cells.is_empty()).then_some(RowChange::Update(cells));
        if prev == next {
            return;
        }
        self.apply(Op { key, identity, prev, next });
    }

    /// Stage a row DELETE, replacing any staged cell edits on it.
    pub fn stage_delete(&mut self, identity: Vec<Value>) {
        let key = key_of(&identity);
        let prev = self.changes.get(&key).map(|e| e.change.clone());
        if matches!(prev, Some(RowChange::Delete)) {
            return;
        }
        self.apply(Op { key, identity, prev, next: Some(RowChange::Delete) });
    }

    /// Discard one row's staged change (the review popover's per-entry
    /// action). Itself undoable.
    pub fn discard(&mut self, key: &str) {
        let Some(entry) = self.changes.get(key) else { return };
        let op = Op {
            key: key.to_string(),
            identity: entry.identity.clone(),
            prev: Some(entry.change.clone()),
            next: None,
        };
        self.apply(op);
    }

    pub fn undo(&mut self) -> bool {
        let Some(op) = self.undo.pop() else { return false };
        match &op.prev {
            Some(change) => {
                self.changes.insert(
                    op.key.clone(),
                    Entry { identity: op.identity.clone(), change: change.clone() },
                );
            }
            None => {
                self.changes.remove(&op.key);
            }
        }
        self.redo.push(op);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(op) = self.redo.pop() else { return false };
        match &op.next {
            Some(change) => {
                self.changes.insert(
                    op.key.clone(),
                    Entry { identity: op.identity.clone(), change: change.clone() },
                );
            }
            None => {
                self.changes.remove(&op.key);
            }
        }
        self.undo.push(op);
        true
    }

    /// Everything is committed or nothing is: clear after a successful
    /// transaction. The undo stack clears with it — commit is the line
    /// of no return, and the grammar says so out loud.
    pub fn clear(&mut self) {
        self.changes.clear();
        self.undo.clear();
        self.redo.clear();
    }

    /// The staged set as parameterized statements, updates before
    /// deletes, deterministic order. The WHERE binds the ORIGINAL key
    /// values — a row's identity is what we fetched, not what we typed.
    pub fn statements(&self) -> Vec<(String, Vec<Value>)> {
        let mut out = Vec::new();
        let where_clause = self
            .pk_cols
            .iter()
            .map(|c| format!("{} = ?", qident(c)))
            .collect::<Vec<_>>()
            .join(" AND ");
        for (_, identity, change) in self.entries() {
            if let RowChange::Update(cells) = change {
                let set = cells
                    .keys()
                    .map(|ix| format!("{} = ?", qident(self.column_name(*ix))))
                    .collect::<Vec<_>>()
                    .join(", ");
                let mut params: Vec<Value> =
                    cells.values().map(|c| c.value.clone()).collect();
                params.extend(identity.iter().cloned());
                out.push((
                    format!("UPDATE {} SET {} WHERE {}", self.source, set, where_clause),
                    params,
                ));
            }
        }
        for (_, identity, change) in self.entries() {
            if matches!(change, RowChange::Delete) {
                out.push((
                    format!("DELETE FROM {} WHERE {}", self.source, where_clause),
                    identity.to_vec(),
                ));
            }
        }
        out
    }
}

/// Quote an identifier the DuckDB way.
fn qident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Text-ish columns are where `''` is a value in its own right; clearing
/// any other type means NULL (docs/EDITING.md, "type-honest clear").
pub fn is_text_type(duck_type: &str) -> bool {
    let ty = duck_type.to_uppercase();
    ty.contains("VARCHAR") || ty.contains("CHAR") || ty == "UUID" || ty == "ENUM"
}

/// Stage-time validation: user text -> the value the statement binds.
/// Cheap errors die closest to the fingers; CHECK/FK/UNIQUE stay the
/// server's verdict at commit. `None` text means NULL.
pub fn parse_value(text: &str, duck_type: &str) -> Result<Value, String> {
    let ty = duck_type.to_uppercase();
    let is_text = is_text_type(&ty);
    // Typing the literal `null` into a non-text column means SQL NULL —
    // it was never a valid INTEGER anyway (DataGrip precedent). In text
    // columns it stores the four characters.
    if !is_text && text.eq_ignore_ascii_case("null") {
        return Ok(Value::Null);
    }
    if ty.contains("INT") {
        return text
            .trim()
            .parse::<i64>()
            .map(Value::from)
            .map_err(|_| format!("{text:?} is not {duck_type}"));
    }
    if ty.starts_with("DOUBLE") || ty.starts_with("FLOAT") || ty.starts_with("REAL") {
        return text
            .trim()
            .parse::<f64>()
            .map(Value::from)
            .map_err(|_| format!("{text:?} is not {duck_type}"));
    }
    if ty.starts_with("DECIMAL") || ty.starts_with("NUMERIC") {
        // Bound as text so precision survives JSON; DuckDB casts.
        return match text.trim().parse::<f64>() {
            Ok(_) => Ok(Value::String(text.trim().to_string())),
            Err(_) => Err(format!("{text:?} is not {duck_type}")),
        };
    }
    if ty == "BOOLEAN" {
        return match text.trim().to_ascii_lowercase().as_str() {
            "true" | "t" | "1" | "yes" => Ok(Value::Bool(true)),
            "false" | "f" | "0" | "no" => Ok(Value::Bool(false)),
            _ => Err(format!("{text:?} is not BOOLEAN")),
        };
    }
    // Dates, timestamps, blobs, nested types: bind the text and let the
    // engine cast — its error comes back atomically at commit.
    Ok(Value::String(text.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn edits() -> Edits {
        Edits::new(
            "\"main\".\"t\"".into(),
            vec!["id".into()],
            vec!["id".into(), "name".into(), "qty".into()],
        )
    }

    fn txt(s: &str) -> Option<SharedString> {
        Some(SharedString::from(s.to_string()))
    }

    #[test]
    fn a_cell_edited_back_to_its_original_auto_cleans() {
        let mut e = edits();
        e.stage_cell(vec![json!(1)], 1, txt("a"), txt("b"), json!("b"));
        assert_eq!(e.counts(), (1, 0));
        e.stage_cell(vec![json!(1)], 1, txt("a"), txt("a"), json!("a"));
        assert!(e.is_empty(), "diff, not log: equal-to-original leaves no entry");
    }

    #[test]
    fn one_entry_per_cell_last_wins_and_undo_walks_back() {
        let mut e = edits();
        e.stage_cell(vec![json!(1)], 1, txt("a"), txt("b"), json!("b"));
        e.stage_cell(vec![json!(1)], 1, txt("a"), txt("c"), json!("c"));
        let key = key_of(&[json!(1)]);
        assert_eq!(e.staged_text(&key, 1), Some(txt("c")));
        assert!(e.undo());
        assert_eq!(e.staged_text(&key, 1), Some(txt("b")));
        assert!(e.undo());
        assert!(e.is_empty());
        assert!(e.redo());
        assert_eq!(e.staged_text(&key, 1), Some(txt("b")));
    }

    #[test]
    fn a_discard_is_itself_undoable() {
        let mut e = edits();
        e.stage_cell(vec![json!(1)], 1, txt("a"), txt("b"), json!("b"));
        let key = key_of(&[json!(1)]);
        e.discard(&key);
        assert!(e.is_empty());
        assert!(e.undo(), "nothing is more than one keystroke from recovery");
        assert_eq!(e.staged_text(&key, 1), Some(txt("b")));
    }

    #[test]
    fn statements_bind_original_identity_and_split_verbs() {
        let mut e = edits();
        e.stage_cell(vec![json!(5)], 0, txt("5"), txt("7"), json!(7));
        e.stage_cell(vec![json!(5)], 2, txt("1"), None, Value::Null);
        e.stage_delete(vec![json!(9)]);
        let stmts = e.statements();
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0].0, "UPDATE \"main\".\"t\" SET \"id\" = ?, \"qty\" = ? WHERE \"id\" = ?");
        // A PK edit is just an update: SET binds the new value, WHERE the original.
        assert_eq!(stmts[0].1, vec![json!(7), Value::Null, json!(5)]);
        assert_eq!(stmts[1].0, "DELETE FROM \"main\".\"t\" WHERE \"id\" = ?");
        assert_eq!(stmts[1].1, vec![json!(9)]);
    }

    #[test]
    fn delete_replaces_cell_edits_and_reverts_whole() {
        let mut e = edits();
        e.stage_cell(vec![json!(1)], 1, txt("a"), txt("b"), json!("b"));
        e.stage_delete(vec![json!(1)]);
        let key = key_of(&[json!(1)]);
        assert!(e.is_deleted(&key));
        assert_eq!(e.counts(), (0, 1));
        assert!(e.undo());
        assert!(!e.is_deleted(&key));
        assert_eq!(e.staged_text(&key, 1), Some(txt("b")));
    }

    #[test]
    fn parse_is_type_honest() {
        assert_eq!(parse_value("42", "INTEGER").unwrap(), json!(42));
        assert!(parse_value("abc", "INTEGER").is_err());
        assert_eq!(parse_value("null", "INTEGER").unwrap(), Value::Null);
        assert_eq!(parse_value("null", "VARCHAR").unwrap(), json!("null"));
        assert_eq!(parse_value("19.99", "DECIMAL(10,2)").unwrap(), json!("19.99"));
        assert_eq!(parse_value("true", "BOOLEAN").unwrap(), json!(true));
    }
}
