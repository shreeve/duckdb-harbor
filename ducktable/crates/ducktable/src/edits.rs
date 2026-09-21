//! The staging layer (docs/EDITING.md): every change the user has made
//! and not yet committed, keyed by row identity — the primary-key
//! columns' original fetched values — never by grid position. The view
//! can sort, filter, and page freely; nothing here moves.
//!
//! This module is pure model: no GPUI, no HTTP. The grid projects it
//! onto the current page for rendering; commit turns it into
//! parameterized statements. Every gesture — including a discard — is
//! one entry on the undo stack, so nothing is ever more than one
//! keystroke from recovery. A gesture that touches many rows (⌘⌫ over a
//! selection, discard-all) is one entry too: the rows stay separate
//! changes for review, and one ⌘Z takes the whole gesture back.

use gpui::SharedString;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

/// One cell's staged change. Display text and what the statement supplies
/// are both kept: the text is what render shows and what auto-clean
/// compares; the bind is what reaches the engine.
#[derive(Clone, Debug, PartialEq)]
pub struct CellEdit {
    /// The fetched display text this edit replaces (None = NULL).
    pub original: Option<SharedString>,
    /// The staged display text (None = NULL).
    pub text: Option<SharedString>,
    /// What the statement puts in the column.
    pub bind: Bind,
    /// The text a duplicate's cell was copied with (inner None = NULL), kept
    /// while the cell is typed over: a cell that holds this text again is
    /// read from the source row again. None on a cell that copies nothing.
    pub copied: Option<Option<SharedString>>,
}

/// Where a staged cell's value comes from.
#[derive(Clone, Debug, PartialEq)]
pub enum Bind {
    /// A value bound through the column's placeholder (Null for NULL).
    Value(Value),
    /// The column as the entry's source row holds it in the database, read
    /// in SQL and never bound: a duplicate's untouched cell. The wire is
    /// narrower than the engine — a VARIANT crosses it as JSON, a MAP as
    /// pairs, an INTERVAL as an object — so a value that went out and came
    /// back would not be the value that was there.
    Source,
}

/// One row's staged fate.
#[derive(Clone, Debug, PartialEq)]
pub enum RowChange {
    /// A not-yet-persisted row. Missing columns mean SQL DEFAULT; a
    /// present cell means the user explicitly supplied a value or NULL.
    Insert(BTreeMap<usize, CellEdit>),
    /// Schema column index -> staged cell.
    Update(BTreeMap<usize, CellEdit>),
    Delete,
}

/// How commit verifies one generated statement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatementExpectation {
    /// UPDATE/DELETE return DuckDB's one-cell affected-row count.
    AffectedOne,
    /// INSERT ... RETURNING * must return exactly one row.
    ReturnedOne,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Statement {
    pub sql: String,
    pub params: Vec<Value>,
    pub expectation: StatementExpectation,
}

/// One row mutation: the row entry's state before and after. Undo
/// restores `prev`, redo restores `next` — one uniform shape for edits,
/// clears, deletes, and discards. An undo step is a group of these:
/// usually one, many for a gesture over many rows.
struct Op {
    key: String,
    identity: Vec<Value>,
    prev: Option<RowChange>,
    next: Option<RowChange>,
}

struct Entry {
    /// The persisted row the statement names in its WHERE: the row an
    /// UPDATE or DELETE changes, the row a duplicate INSERT reads its
    /// `Bind::Source` cells from. Empty for a draft that copies nothing.
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
    /// Each column's DuckDB type, parallel to `columns`: what decides how
    /// its value is bound (`placeholder`).
    types: Vec<String>,
    changes: HashMap<String, Entry>,
    undo: Vec<Vec<Op>>,
    redo: Vec<Vec<Op>>,
    next_draft: u64,
}

/// A row identity's map key: its canonical JSON. Values compare by
/// serialization, which is exactly the equality the wire speaks.
pub fn key_of(identity: &[Value]) -> String {
    serde_json::to_string(identity).unwrap_or_default()
}

impl Edits {
    pub fn new(source: String, pk_cols: Vec<String>, columns: Vec<String>, types: Vec<String>) -> Self {
        Self {
            source,
            pk_cols,
            columns,
            types,
            changes: HashMap::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            next_draft: 1,
        }
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// The quoted `"schema"."table"` these changes target.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// True when another staging set targets the same table with the
    /// same key and column layout — the safety check for handing edits
    /// across grid rebuilds (Law 4: staged changes belong to the table,
    /// not the view).
    pub fn same_shape(&self, other: &Edits) -> bool {
        self.source == other.source
            && self.pk_cols == other.pk_cols
            && self.columns == other.columns
            && self.types == other.types
    }

    /// (inserts, updates, deletes) — the verb-split status line.
    pub fn counts(&self) -> (usize, usize, usize) {
        let inserts = self
            .changes
            .values()
            .filter(|e| matches!(e.change, RowChange::Insert(_)))
            .count();
        let deletes = self
            .changes
            .values()
            .filter(|e| matches!(e.change, RowChange::Delete))
            .count();
        (inserts, self.changes.len() - inserts - deletes, deletes)
    }

    /// The staged display text for a cell, if any. `Some(None)` means
    /// staged NULL.
    #[cfg(test)]
    pub fn staged_text(&self, key: &str, col: usize) -> Option<Option<SharedString>> {
        match &self.changes.get(key)?.change {
            RowChange::Insert(cells) | RowChange::Update(cells) => {
                cells.get(&col).map(|c| c.text.clone())
            }
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

    /// The first INSERT cell DuckTable can prove is missing before it
    /// asks the server. CHECK/UNIQUE/FK remain DuckDB's verdict.
    pub fn first_missing_required(
        &self,
        not_null: &[bool],
        defaults: &[Option<String>],
        generated: &[bool],
    ) -> Option<(String, usize)> {
        self.entries().into_iter().find_map(|(key, _, change)| {
            let RowChange::Insert(cells) = change else { return None };
            not_null.iter().enumerate().find_map(|(col, required)| {
                (*required
                    && !generated.get(col).copied().unwrap_or(false)
                    && defaults.get(col).and_then(Option::as_ref).is_none()
                    && !cells.contains_key(&col))
                .then(|| (key.to_string(), col))
            })
        })
    }

    /// Add an intentional all-DEFAULT draft. It is staged immediately:
    /// DEFAULT VALUES can itself be a valid insert, and one undo removes it.
    pub fn stage_insert(&mut self) -> String {
        self.stage_duplicate(Vec::new(), Vec::new())
    }

    /// Add a draft copied from the persisted row `source`. Each cell shows
    /// `text`; a `Bind::Source` cell is read from that row when the INSERT
    /// runs, and a `Bind::Value` cell is bound like any typed one. The
    /// whole copied row is one undo step, just as an empty New Row is.
    pub fn stage_duplicate(
        &mut self,
        source: Vec<Value>,
        cells: Vec<(usize, Option<SharedString>, Bind)>,
    ) -> String {
        let key = format!("draft:{:020}", self.next_draft);
        self.next_draft += 1;
        let cells = cells
            .into_iter()
            .map(|(col, text, bind)| {
                let copied = (bind == Bind::Source).then(|| text.clone());
                (col, CellEdit { original: None, text, bind, copied })
            })
            .collect();
        self.apply(Op {
            key: key.clone(),
            identity: source,
            prev: None,
            next: Some(RowChange::Insert(cells)),
        });
        key
    }

    /// Supply one draft cell. `text = None` is explicit SQL NULL; an
    /// untouched cell is DEFAULT and is absent from the map. A duplicate's
    /// cell given the text it was copied with is read from the source row,
    /// not bound: the text is all the wire kept of a DATE inside a VARIANT
    /// or an integer past 64 bits, and the source row still holds the value.
    pub fn stage_insert_cell(
        &mut self,
        key: &str,
        col: usize,
        text: Option<SharedString>,
        value: Value,
    ) {
        let Some(entry) = self.changes.get(key) else { return };
        let RowChange::Insert(mut cells) = entry.change.clone() else { return };
        let prev = Some(entry.change.clone());
        let copied = cells.get(&col).and_then(|c| c.copied.clone());
        let bind = if copied.as_ref() == Some(&text) { Bind::Source } else { Bind::Value(value) };
        cells.insert(col, CellEdit { original: None, text, bind, copied });
        let next = Some(RowChange::Insert(cells));
        if prev == next {
            return;
        }
        self.apply(Op { key: key.to_string(), identity: entry.identity.clone(), prev, next });
    }

    /// The text a duplicate's cell was copied with, for a cell that was
    /// (`Some(None)` = copied NULL).
    pub fn copied_text(&self, key: &str, col: usize) -> Option<Option<SharedString>> {
        match &self.changes.get(key)?.change {
            RowChange::Insert(cells) => cells.get(&col)?.copied.clone(),
            _ => None,
        }
    }

    /// Put a duplicate's cell back to the text it was copied with, read
    /// from the source row.
    pub fn stage_insert_copied(&mut self, key: &str, col: usize) {
        if let Some(text) = self.copied_text(key, col) {
            self.stage_insert_cell(key, col, text, Value::Null);
        }
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
        self.undo.push(vec![op]);
        self.redo.clear();
    }

    /// Run a gesture over many rows as ONE undo step. Every mutation the
    /// closure makes lands on the stack as usual; afterwards they are
    /// folded into a single group, so ⌘Z takes the gesture back whole
    /// and ⌘⇧Z replays it whole. A gesture of one mutation, or none,
    /// leaves the stack exactly as the mutations did.
    pub fn grouped(&mut self, f: impl FnOnce(&mut Self)) {
        let start = self.undo.len();
        f(self);
        if self.undo.len() > start + 1 {
            let ops: Vec<Op> = self.undo.drain(start..).flatten().collect();
            self.undo.push(ops);
        }
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
            cells.insert(col, CellEdit { original, text, bind: Bind::Value(value), copied: None });
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
        let Some(group) = self.undo.pop() else { return false };
        // Walked backwards: within a group the same row may appear
        // twice, and its earlier state must be the one that stands.
        for op in group.iter().rev() {
            self.restore(&op.key, &op.identity, &op.prev);
        }
        self.redo.push(group);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(group) = self.redo.pop() else { return false };
        for op in &group {
            self.restore(&op.key, &op.identity, &op.next);
        }
        self.undo.push(group);
        true
    }

    fn restore(&mut self, key: &str, identity: &[Value], change: &Option<RowChange>) {
        match change {
            Some(change) => {
                self.changes.insert(
                    key.to_string(),
                    Entry { identity: identity.to_vec(), change: change.clone() },
                );
            }
            None => {
                self.changes.remove(key);
            }
        }
    }

    /// Everything is committed or nothing is: clear after a successful
    /// transaction. The undo stack clears with it — commit is the line
    /// of no return, and the grammar says so out loud.
    pub fn clear(&mut self) {
        self.changes.clear();
        self.undo.clear();
        self.redo.clear();
    }

    /// The staged set as parameterized statements: inserts, updates,
    /// deletes, deterministic within each verb. Missing insert columns
    /// stay out of the statement so DuckDB supplies DEFAULT. The WHERE
    /// binds the ORIGINAL key values for existing rows.
    ///
    /// A duplicate with `Bind::Source` cells selects them from its source
    /// row, so the engine copies what the wire could not carry. Inserts
    /// run before any update or delete, so that row is read as the
    /// database holds it, whatever else is staged on it; and a source row
    /// that is gone returns no row, which commit refuses.
    pub fn statements(&self) -> Vec<Statement> {
        let mut out = Vec::new();
        let where_clause = self
            .pk_cols
            .iter()
            .map(|c| format!("{} = {}", qident(c), self.key_placeholder(c)))
            .collect::<Vec<_>>()
            .join(" AND ");
        for (_, identity, change) in self.entries() {
            if let RowChange::Insert(cells) = change {
                let mut params = Vec::new();
                let sql = if cells.is_empty() {
                    format!("INSERT INTO {} DEFAULT VALUES RETURNING *", self.source)
                } else {
                    let names = cells
                        .keys()
                        .map(|ix| qident(self.column_name(*ix)))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let supplied = cells
                        .iter()
                        .map(|(ix, cell)| self.supply(*ix, cell, &mut params))
                        .collect::<Vec<_>>()
                        .join(", ");
                    if cells.values().any(|c| c.bind == Bind::Source) {
                        params.extend(identity.iter().cloned());
                        format!(
                            "INSERT INTO {} ({names}) SELECT {supplied} FROM {} WHERE {where_clause} RETURNING *",
                            self.source, self.source
                        )
                    } else {
                        format!("INSERT INTO {} ({names}) VALUES ({supplied}) RETURNING *", self.source)
                    }
                };
                out.push(Statement {
                    sql,
                    params,
                    expectation: StatementExpectation::ReturnedOne,
                });
            }
        }
        for (_, identity, change) in self.entries() {
            if let RowChange::Update(cells) = change {
                let mut params = Vec::new();
                let set = cells
                    .iter()
                    .map(|(ix, cell)| {
                        format!("{} = {}", qident(self.column_name(*ix)), self.supply(*ix, cell, &mut params))
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                params.extend(identity.iter().cloned());
                out.push(Statement {
                    sql: format!("UPDATE {} SET {} WHERE {}", self.source, set, where_clause),
                    params,
                    expectation: StatementExpectation::AffectedOne,
                });
            }
        }
        for (_, identity, change) in self.entries() {
            if matches!(change, RowChange::Delete) {
                out.push(Statement {
                    sql: format!("DELETE FROM {} WHERE {}", self.source, where_clause),
                    params: identity.to_vec(),
                    expectation: StatementExpectation::AffectedOne,
                });
            }
        }
        out
    }

    /// The SQL that supplies column `ix` from `cell`, pushing what it
    /// binds: the column's placeholder for a value, the column itself for
    /// a cell read from the row the statement's WHERE names.
    fn supply(&self, ix: usize, cell: &CellEdit, params: &mut Vec<Value>) -> String {
        match &cell.bind {
            Bind::Value(value) => {
                params.push(value.clone());
                self.placeholder(ix).to_string()
            }
            Bind::Source => qident(self.column_name(ix)),
        }
    }

    /// How column `ix`'s value is bound.
    fn placeholder(&self, ix: usize) -> &'static str {
        placeholder_for(self.types.get(ix).map(String::as_str).unwrap_or(""))
    }

    /// How a key column, known by name, is bound in a WHERE.
    fn key_placeholder(&self, name: &str) -> &'static str {
        let ty = self.columns.iter().position(|c| c == name).and_then(|ix| self.types.get(ix));
        key_placeholder_for(ty.map(String::as_str).unwrap_or(""))
    }
}

/// The placeholder that carries a value of this type to the engine as the
/// value it is. Harbor binds text as VARCHAR, and for most types the
/// engine's cast from VARCHAR is the right one. Two are not. JSON text cast
/// to VARIANT is a VARIANT *string* — every path into it NULL, nothing said
/// — so a document goes in through JSON, which is also how Harbor sent it
/// out. A BLOB arrives as base64, and its characters cast to BLOB are those
/// characters' bytes, so it is decoded on the way back; the inner cast
/// gives a NULL a type `from_base64` accepts.
fn placeholder_for(duck_type: &str) -> &'static str {
    match duck_type.to_uppercase().as_str() {
        "VARIANT" | "JSON" => "?::JSON",
        "BLOB" => "from_base64(?::VARCHAR)",
        _ => "?",
    }
}

/// The placeholder that compares a key column with its fetched value. A
/// FLOAT crosses the wire as the shortest decimal that names it, and a JSON
/// number binds as a DOUBLE. Compared bare, the column is widened to meet
/// it, and 1.1 the FLOAT is not 1.1 the DOUBLE: the WHERE names no row. Cast
/// to FLOAT, the param rounds to the key it came from. A SET or VALUES list
/// needs no such cast, because assignment does that rounding itself.
fn key_placeholder_for(duck_type: &str) -> &'static str {
    match type_head(&duck_type.to_uppercase()) {
        "FLOAT" | "FLOAT4" | "REAL" => "?::FLOAT",
        _ => placeholder_for(duck_type),
    }
}

/// Quote an identifier the DuckDB way.
fn qident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Text columns are where `''` is a value in its own right; clearing any
/// other type means NULL (docs/EDITING.md, "type-honest clear"). An ENUM
/// and a UUID read like text and are not: the engine refuses `''` for
/// both, so clearing one is NULL. Nor is a container of text — a
/// `VARCHAR[]`, a `STRUCT(name VARCHAR)` — whose cleared cell is NULL too.
pub fn is_text_type(duck_type: &str) -> bool {
    let ty = duck_type.to_uppercase();
    matches!(type_head(&ty), "VARCHAR" | "NVARCHAR" | "CHAR" | "BPCHAR" | "TEXT" | "STRING")
}

/// Whether whitespace around a cell's text is part of its value. It is for
/// text; for a JSON column, which keeps its text character for character;
/// and for an ENUM, whose values are strings (`ENUM('a', ' a')` has two).
/// For every other type the engine's cast reads ` 5 `, ` 2024-02-29 ` and
/// ` [1, 2] ` as it reads them bare, or refuses the padded text outright (a
/// UUID, a BIT, base64), so padding never names a different value.
fn keeps_whitespace(duck_type: &str) -> bool {
    let ty = duck_type.to_uppercase();
    is_text_type(&ty) || matches!(type_head(&ty), "JSON" | "ENUM")
}

/// A scalar type's own name, without its parameters: `DECIMAL(10,2)` is
/// `DECIMAL`, `ENUM('a', 'b')` is `ENUM`. A container is never the name of
/// what it contains: `STRUCT(a INTEGER)` is `STRUCT`, and `INTEGER[]` and
/// `DECIMAL(10,2)[]` stay whole, matching no scalar. Harbor sends `VARCHAR`,
/// `INTEGER`, `DOUBLE` and the like (its `type_name`); the aliases matched
/// beside them cost nothing.
fn type_head(ty: &str) -> &str {
    match ty.find('(') {
        Some(at) if ty.ends_with(')') => ty[..at].trim_end(),
        _ => ty,
    }
}

/// An integer type's range, lowest and highest. The highest is a u128
/// because UHUGEINT's is past i128.
fn integer_bounds(name: &str) -> Option<(i128, u128)> {
    Some(match name {
        "TINYINT" | "INT1" => (i8::MIN as i128, i8::MAX as u128),
        "SMALLINT" | "INT2" | "INT16" | "SHORT" => (i16::MIN as i128, i16::MAX as u128),
        "INTEGER" | "INT4" | "INT32" | "INT" | "SIGNED" => (i32::MIN as i128, i32::MAX as u128),
        "BIGINT" | "INT8" | "INT64" | "LONG" => (i64::MIN as i128, i64::MAX as u128),
        "HUGEINT" | "INT128" => (i128::MIN, i128::MAX as u128),
        "UTINYINT" | "UINT8" => (0, u8::MAX as u128),
        "USMALLINT" | "UINT16" => (0, u16::MAX as u128),
        "UINTEGER" | "UINT32" => (0, u32::MAX as u128),
        "UBIGINT" | "UINT64" => (0, u64::MAX as u128),
        "UHUGEINT" | "UINT128" => (0, u128::MAX),
        _ => return None,
    })
}

/// Integer text -> its bind value, refused when it is not an integer or
/// not in the type's range. One that fits an i64 is a JSON number. A wider
/// one is bound as its digits, which the engine casts exactly: a JSON
/// number that wide is a double before it arrives.
fn parse_integer(text: &str, duck_type: &str, (min, max): (i128, u128)) -> Result<Value, String> {
    let t = text.trim();
    let (negative, digits) = match t.strip_prefix('-') {
        Some(digits) => (true, digits),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("{text:?} is not {duck_type}"));
    }
    // Digits that overflow a u128 are past every type's range.
    let in_range = digits
        .parse::<u128>()
        .is_ok_and(|n| if negative { n <= min.unsigned_abs() } else { n <= max });
    if !in_range {
        return Err(format!("{text:?} is out of range for {duck_type}"));
    }
    Ok(match t.parse::<i64>() {
        Ok(n) => Value::from(n),
        Err(_) => Value::String(format!("{}{digits}", if negative { "-" } else { "" })),
    })
}

/// A cell as the editor finds it when it confirms. `None` text is NULL.
pub struct Held<'a> {
    /// The column's DuckDB type.
    pub ty: &'a str,
    /// A draft row's cell, not a persisted row's.
    pub draft: bool,
    /// What the database holds. A draft has nothing fetched.
    pub fetched: Option<&'a str>,
    /// The staged cell, or the draft's cell, when there is one. A draft
    /// without one is DEFAULT.
    pub staged: Option<Option<&'a str>>,
    /// The text a duplicate's cell was copied with (`CellEdit::copied`).
    pub copied: Option<Option<&'a str>>,
}

/// What confirming an editor does to its cell.
#[derive(Debug, PartialEq)]
pub enum Confirm {
    /// The cell already holds this text. Nothing is staged and nothing is
    /// validated, so a value the engine accepted is never one the editor
    /// refuses to leave.
    Keep,
    /// The text the cell had before anyone typed in it: what was fetched,
    /// or what a duplicate copied. The staged edit is dropped, or the cell
    /// is read from its source row again; neither needs a verdict.
    Revert,
    /// Stage this text and bind this value. `None` is NULL, which a NOT
    /// NULL column refuses.
    Stage(Option<SharedString>, Value),
    /// The text is not a value of the column's type; the reason.
    Refuse(String),
}

/// Decide what confirming `text` over `cell` does. An editor cannot tell
/// NULL from the empty string — both open empty — so an empty editor over a
/// cell that holds NULL, or over a draft's DEFAULT, is that cell unchanged:
/// NULL in, nothing typed, NULL out. `''` is entered by emptying a text cell
/// that held something, or with Delete.
///
/// Whitespace around the text is compared only where it is part of the value
/// (`keeps_whitespace`): ` 5` over an INTEGER that holds `5` is that cell
/// unchanged, and ` 5` over a VARCHAR that holds `5` is another string.
pub fn confirm(text: &str, cell: &Held) -> Confirm {
    let exact = keeps_whitespace(cell.ty);
    let same = |held: Option<&str>| {
        let held = held.unwrap_or("");
        if exact { held == text } else { held.trim() == text.trim() }
    };
    if same(cell.staged.unwrap_or(cell.fetched)) {
        return Confirm::Keep;
    }
    let before = if cell.draft { cell.copied } else { Some(cell.fetched) };
    if before.is_some_and(same) {
        return Confirm::Revert;
    }
    if text.is_empty() {
        // An emptied editor: '' for text (the one honest way to enter it),
        // NULL for everything else — docs/EDITING.md.
        return if is_text_type(cell.ty) {
            Confirm::Stage(Some(SharedString::from("")), Value::String(String::new()))
        } else {
            Confirm::Stage(None, Value::Null)
        };
    }
    match parse_value(text, cell.ty) {
        Ok(Value::Null) => Confirm::Stage(None, Value::Null),
        Ok(value) => Confirm::Stage(Some(SharedString::from(text.to_string())), value),
        Err(reason) => Confirm::Refuse(reason),
    }
}

/// Stage-time validation: user text -> the value the statement binds.
/// Cheap errors die closest to the fingers; CHECK/FK/UNIQUE stay the
/// server's verdict at commit. `None` text means NULL.
pub fn parse_value(text: &str, duck_type: &str) -> Result<Value, String> {
    let ty = duck_type.to_uppercase();
    let is_text = is_text_type(&ty);
    // A JSON column holds JSON text, and `null` is a JSON value there,
    // distinct from SQL NULL — so this comes before the `null` rule below.
    if ty == "JSON" {
        check_json(text)?;
        return Ok(Value::String(text.to_string()));
    }
    // Typing the literal `null` into a non-text column means SQL NULL —
    // it was never a valid INTEGER anyway (DataGrip precedent). In text
    // columns it stores the four characters. In a BLOB cell it is base64
    // like any other text there: `null`, `NULL` and `Null` each decode to
    // three bytes (9EE965, 3542CB, 36E965), and bytes a cell can show are
    // bytes it can take. A BLOB's NULL is entered with ⌃⇧N or Delete.
    if !is_text && ty != "BLOB" && text.eq_ignore_ascii_case("null") {
        return Ok(Value::Null);
    }
    // A VARIANT cell is JSON text both ways (`placeholder_for`). Its
    // top-level `null` is SQL NULL to the engine, which the rule above
    // already said. The text is bound as typed, never re-serialized: a
    // round trip through serde would rewrite 12.340 and any wide integer.
    if ty == "VARIANT" {
        check_json(text)?;
        return Ok(Value::String(text.to_string()));
    }
    // A container that holds a VARIANT, a JSON or a BLOB is bound as the
    // container's text, and no cast of that text reaches the inner value: a
    // `BLOB[]` stores the base64 characters as the bytes, and the elements of
    // a `VARIANT[]` or a `JSON[]` become strings. Nothing is said, so the edit
    // is refused here; `null` above, and NULL from an emptied cell, are safe.
    if ty != "BLOB" {
        if let Some(inner) = document_or_blob_within(&ty) {
            return Err(format!(
                "typed text cannot carry the {inner} inside {duck_type} \u{2014} edit this cell in the Query tab"
            ));
        }
    }
    // Every test below is on the scalar's own name, so a nested type —
    // `INTEGER[]`, `STRUCT(a INTEGER)`, `MAP(VARCHAR, INTEGER)` — and a
    // type whose spelling happens to hold another's — INTERVAL, an
    // `ENUM('POINT')` — fall through to the engine's cast at the end.
    let head = type_head(&ty);
    if let Some(bounds) = integer_bounds(head) {
        return parse_integer(text, duck_type, bounds);
    }
    if matches!(head, "DOUBLE" | "FLOAT8" | "FLOAT" | "FLOAT4" | "REAL") {
        let t = text.trim();
        // A FLOAT is 32 bits. The engine rounds the DOUBLE it is handed to
        // the nearest FLOAT and refuses one that rounds past the largest,
        // which is what this cast does: 3.40282356e38 is still the largest
        // FLOAT, 3.4028236e38 is out of range.
        let single = head != "DOUBLE" && head != "FLOAT8";
        return match t.parse::<f64>() {
            Ok(v) if v.is_finite() && single && !(v as f32).is_finite() => {
                Err(format!("{text:?} is out of range for {duck_type}"))
            }
            Ok(v) if v.is_finite() => Ok(Value::from(v)),
            // NaN and the infinities have no JSON number — serde makes
            // null of them — and the engine reads their names, so the name
            // is what is bound. Without a digit, the text is such a name.
            Ok(_) if !t.bytes().any(|b| b.is_ascii_digit()) => Ok(Value::String(t.to_string())),
            // Digits that parse to infinity are a number too large.
            Ok(_) => Err(format!("{text:?} is out of range for {duck_type}")),
            Err(_) => Err(format!("{text:?} is not {duck_type}")),
        };
    }
    if matches!(head, "DECIMAL" | "NUMERIC") {
        // Bound as text so precision survives JSON; DuckDB casts.
        return match text.trim().parse::<f64>() {
            Ok(v) if v.is_finite() => Ok(Value::String(text.trim().to_string())),
            _ => Err(format!("{text:?} is not {duck_type}")),
        };
    }
    if head == "BOOLEAN" {
        return match text.trim().to_ascii_lowercase().as_str() {
            "true" | "t" | "1" | "yes" => Ok(Value::Bool(true)),
            "false" | "f" | "0" | "no" => Ok(Value::Bool(false)),
            _ => Err(format!("{text:?} is not BOOLEAN")),
        };
    }
    // Dates, timestamps, intervals, enums, blobs, nested types: bind the
    // text and let the engine cast — its error comes back atomically at
    // commit.
    Ok(Value::String(text.to_string()))
}

/// The deepest a document nests, the limit every first-party client keeps.
/// Harbor's request parser refuses an object param a little past it, and deep
/// nesting is the one input that hurts the engine through a VARIANT.
const JSON_DEPTH: usize = 100;

/// A VARIANT or JSON cell takes strict JSON. The engine's own JSON also reads
/// NaN and Infinity as numbers, and a VARIANT stores them; Harbor then sends
/// `{"x":NaN}`, which no JSON reader accepts, and the whole document reaches
/// a client as a string. So serde's verdict is the verdict. Depth is
/// measured first, so that a document refused for its depth is told so.
fn check_json(text: &str) -> Result<(), String> {
    if json_depth(text) > JSON_DEPTH {
        return Err(format!("this JSON nests deeper than {JSON_DEPTH} levels"));
    }
    match serde_json::from_str::<Value>(text) {
        Ok(_) => Ok(()),
        Err(_) => Err(format!("{text:?} is not JSON \u{2014} text needs quotes, like \"Morel\"")),
    }
}

/// The most brackets open at once in `text`, outside any string.
fn json_depth(text: &str) -> usize {
    let (mut depth, mut deepest) = (0usize, 0usize);
    let (mut in_string, mut escaped) = (false, false);
    for c in text.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '[' | '{' => {
                depth += 1;
                deepest = deepest.max(depth);
            }
            ']' | '}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    deepest
}

/// The VARIANT, JSON or BLOB a container type holds — `BLOB[]`,
/// `STRUCT(v VARIANT)`, `MAP(VARCHAR, JSON)` — if it holds one. A word of the
/// type counts unless it is quoted (an ENUM's values, a quoted field name) or
/// is the field name that opens a STRUCT or UNION member.
fn document_or_blob_within(duck_type: &str) -> Option<&'static str> {
    let ty = duck_type.to_uppercase();
    // Per open parenthesis: whether its members are written `name TYPE`.
    let mut named = Vec::new();
    let mut expect_name = false;
    let mut word = String::new();
    let mut last_word = String::new();
    let mut quote = None;
    for c in ty.chars().chain(std::iter::once(' ')) {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        if c.is_alphanumeric() || c == '_' {
            word.push(c);
            continue;
        }
        if !word.is_empty() {
            if expect_name {
                expect_name = false;
            } else if let Some(found) = ["VARIANT", "JSON", "BLOB"].into_iter().find(|t| *t == word) {
                return Some(found);
            }
            last_word = std::mem::take(&mut word);
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                // A quoted run here is the member's name.
                expect_name = false;
            }
            '(' => {
                named.push(matches!(last_word.as_str(), "STRUCT" | "UNION"));
                expect_name = named.last().copied().unwrap_or(false);
            }
            ')' => {
                named.pop();
            }
            ',' => expect_name = named.last().copied().unwrap_or(false),
            _ => {}
        }
    }
    None
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
            vec!["INTEGER".into(), "VARCHAR".into(), "INTEGER".into()],
        )
    }

    fn txt(s: &str) -> Option<SharedString> {
        Some(SharedString::from(s.to_string()))
    }

    #[test]
    fn a_cell_edited_back_to_its_original_auto_cleans() {
        let mut e = edits();
        e.stage_cell(vec![json!(1)], 1, txt("a"), txt("b"), json!("b"));
        assert_eq!(e.counts(), (0, 1, 0));
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
        assert_eq!(stmts[0].sql, "UPDATE \"main\".\"t\" SET \"id\" = ?, \"qty\" = ? WHERE \"id\" = ?");
        // A PK edit is just an update: SET binds the new value, WHERE the original.
        assert_eq!(stmts[0].params, vec![json!(7), Value::Null, json!(5)]);
        assert_eq!(stmts[1].sql, "DELETE FROM \"main\".\"t\" WHERE \"id\" = ?");
        assert_eq!(stmts[1].params, vec![json!(9)]);
    }

    #[test]
    fn delete_replaces_cell_edits_and_reverts_whole() {
        let mut e = edits();
        e.stage_cell(vec![json!(1)], 1, txt("a"), txt("b"), json!("b"));
        e.stage_delete(vec![json!(1)]);
        let key = key_of(&[json!(1)]);
        assert!(e.is_deleted(&key));
        assert_eq!(e.counts(), (0, 0, 1));
        assert!(e.undo());
        assert!(!e.is_deleted(&key));
        assert_eq!(e.staged_text(&key, 1), Some(txt("b")));
    }

    #[test]
    fn a_gesture_over_many_rows_is_one_undo_step() {
        let mut e = edits();
        e.stage_cell(vec![json!(1)], 1, txt("a"), txt("b"), json!("b"));
        e.grouped(|e| {
            e.stage_delete(vec![json!(1)]);
            e.stage_delete(vec![json!(2)]);
            e.stage_delete(vec![json!(3)]);
        });
        assert_eq!(e.counts(), (0, 0, 3));
        // One ⌘Z takes the whole gesture back, and the cell edit the
        // delete had replaced is standing again.
        assert!(e.undo());
        assert_eq!(e.counts(), (0, 1, 0));
        assert_eq!(e.staged_text(&key_of(&[json!(1)]), 1), Some(txt("b")));
        // One ⌘⇧Z replays it whole.
        assert!(e.redo());
        assert_eq!(e.counts(), (0, 0, 3));
        // Then the cell edit is its own step, as before.
        assert!(e.undo());
        assert!(e.undo());
        assert!(e.is_empty());
        assert!(!e.undo());
    }

    #[test]
    fn a_gesture_of_one_or_no_mutations_leaves_the_stack_as_it_was() {
        let mut e = edits();
        e.grouped(|_| {});
        assert!(!e.undo());
        e.grouped(|e| e.stage_delete(vec![json!(1)]));
        e.grouped(|e| e.stage_delete(vec![json!(1)])); // already deleted: no-op
        assert!(e.undo());
        assert!(e.is_empty());
        assert!(!e.undo());
    }

    #[test]
    fn parse_is_type_honest() {
        assert_eq!(parse_value("42", "INTEGER").unwrap(), json!(42));
        assert!(parse_value("abc", "INTEGER").is_err());
        assert_eq!(parse_value("null", "INTEGER").unwrap(), Value::Null);
        assert_eq!(parse_value("null", "VARCHAR").unwrap(), json!("null"));
        assert_eq!(parse_value("19.99", "DECIMAL(10,2)").unwrap(), json!("19.99"));
        assert!(parse_value("nan", "DECIMAL(10,2)").is_err());
        assert_eq!(parse_value("true", "BOOLEAN").unwrap(), json!(true));
    }

    #[test]
    fn a_type_is_matched_by_its_own_name_not_by_what_it_contains() {
        // None of these is an integer, a float, a decimal or text: the text
        // is bound and the engine casts it.
        for (text, ty) in [
            ("3 days", "INTERVAL"),
            ("[1, 2]", "INTEGER[]"),
            ("[1, 2, 3]", "INTEGER[3]"),
            ("{'a': 1, 'b': x}", "STRUCT(a INTEGER, b VARCHAR)"),
            ("{a=1}", "MAP(VARCHAR, INTEGER)"),
            ("POINT", "ENUM('POINT', 'LINE')"),
            ("[1.5, nan]", "DOUBLE[]"),
            ("[19.99]", "DECIMAL(10,2)[]"),
            ("[a, b]", "VARCHAR[]"),
            ("7", "UNION(n INTEGER, s VARCHAR)"),
        ] {
            assert_eq!(parse_value(text, ty), Ok(json!(text)), "{ty}");
            assert!(!is_text_type(ty), "{ty}");
            // Not text, so `null` is SQL NULL, and a cleared cell is NULL.
            assert_eq!(parse_value("null", ty), Ok(Value::Null), "{ty}");
        }
        for ty in ["VARCHAR", "varchar", "VARCHAR(10)", "CHAR(3)", "TEXT"] {
            assert!(is_text_type(ty), "{ty}");
            assert_eq!(parse_value("null", ty), Ok(json!("null")), "{ty}");
        }
        // The engine refuses '' for both, so neither clears to ''.
        assert!(!is_text_type("UUID"));
        assert!(!is_text_type("ENUM('a', 'b')"));
    }

    #[test]
    fn an_integer_is_held_to_its_range_and_bound_as_text_past_i64() {
        for ty in ["TINYINT", "SMALLINT", "INTEGER", "BIGINT", "HUGEINT", "INT", "int8"] {
            assert_eq!(parse_value(" -7 ", ty), Ok(json!(-7)), "{ty}");
            assert_eq!(parse_value("+7", ty), Ok(json!(7)), "{ty}");
        }
        for ty in ["UTINYINT", "USMALLINT", "UINTEGER", "UBIGINT", "UHUGEINT"] {
            assert_eq!(parse_value("7", ty), Ok(json!(7)), "{ty}");
            assert_eq!(parse_value("-0", ty), Ok(json!(0)), "{ty}");
            let err = parse_value("-1", ty).unwrap_err();
            assert!(err.contains("out of range"), "{ty}: {err}");
        }
        for (ty, low, high) in [
            ("TINYINT", "-128", "127"),
            ("UTINYINT", "0", "255"),
            ("SMALLINT", "-32768", "32767"),
            ("INTEGER", "-2147483648", "2147483647"),
            ("UINTEGER", "0", "4294967295"),
            ("BIGINT", "-9223372036854775808", "9223372036854775807"),
        ] {
            assert_eq!(parse_value(low, ty), Ok(json!(low.parse::<i64>().unwrap())), "{ty}");
            assert_eq!(parse_value(high, ty), Ok(json!(high.parse::<i64>().unwrap())), "{ty}");
        }
        assert!(parse_value("128", "TINYINT").unwrap_err().contains("out of range"));
        assert!(parse_value("-129", "TINYINT").unwrap_err().contains("out of range"));
        assert!(parse_value("9223372036854775808", "BIGINT").unwrap_err().contains("out of range"));

        // Past i64 the digits are bound as text, which the engine casts
        // exactly; a JSON number that wide would arrive as a double.
        assert_eq!(parse_value("9223372036854775807", "UBIGINT"), Ok(json!(9223372036854775807i64)));
        assert_eq!(parse_value("9223372036854775808", "UBIGINT"), Ok(json!("9223372036854775808")));
        assert_eq!(parse_value("18446744073709551615", "UBIGINT"), Ok(json!("18446744073709551615")));
        assert!(parse_value("18446744073709551616", "UBIGINT").unwrap_err().contains("out of range"));
        let huge = "170141183460469231731687303715884105727";
        assert_eq!(parse_value(huge, "HUGEINT"), Ok(json!(huge)));
        assert_eq!(parse_value(&format!("+{huge}"), "HUGEINT"), Ok(json!(huge)));
        assert_eq!(
            parse_value("-170141183460469231731687303715884105728", "HUGEINT"),
            Ok(json!("-170141183460469231731687303715884105728"))
        );
        assert!(parse_value("170141183460469231731687303715884105728", "HUGEINT").is_err());
        let uhuge = "340282366920938463463374607431768211455";
        assert_eq!(parse_value(uhuge, "UHUGEINT"), Ok(json!(uhuge)));
        assert!(parse_value("340282366920938463463374607431768211456", "UHUGEINT").is_err());
        assert!(parse_value(&"9".repeat(60), "UHUGEINT").unwrap_err().contains("out of range"));

        for text in ["", "-", "+", "1.5", "1e3", "0x10", "1_000", "12a", "--1"] {
            let err = parse_value(text, "HUGEINT").unwrap_err();
            assert!(err.contains("is not HUGEINT"), "{text}: {err}");
        }
    }

    #[test]
    fn a_double_that_is_not_finite_is_bound_by_name_and_never_as_null() {
        for ty in ["DOUBLE", "FLOAT", "REAL"] {
            assert_eq!(parse_value("1.5", ty), Ok(json!(1.5)), "{ty}");
            assert_eq!(parse_value(" -2e10 ", ty), Ok(json!(-2e10)), "{ty}");
            for name in ["nan", "NaN", "inf", "-inf", "+inf", "Infinity", "-Infinity", "infinity"] {
                assert_eq!(parse_value(name, ty), Ok(json!(name)), "{ty} {name}");
            }
            assert_eq!(parse_value(" -inf ", ty), Ok(json!("-inf")), "{ty}");
            for text in ["1e999", "-1e999"] {
                let err = parse_value(text, ty).unwrap_err();
                assert!(err.contains("out of range"), "{ty} {text}: {err}");
            }
            assert!(parse_value("abc", ty).unwrap_err().contains("is not"));
            assert_eq!(parse_value("null", ty), Ok(Value::Null));
        }
    }

    /// A table whose columns are the three types a bare `?` gets wrong.
    fn typed() -> Edits {
        Edits::new(
            "\"main\".\"t\"".into(),
            vec!["id".into()],
            vec!["id".into(), "doc".into(), "j".into(), "b".into()],
            vec!["INTEGER".into(), "VARIANT".into(), "JSON".into(), "BLOB".into()],
        )
    }

    #[test]
    fn a_document_binds_through_json_and_a_blob_through_base64() {
        let mut e = typed();
        e.stage_cell(vec![json!(1)], 1, txt("{}"), txt("{\"a\":1}"), json!("{\"a\":1}"));
        e.stage_cell(vec![json!(1)], 2, txt("[]"), txt("[1]"), json!("[1]"));
        e.stage_cell(vec![json!(1)], 3, txt("qg=="), txt("qrs="), json!("qrs="));
        let draft = e.stage_insert();
        e.stage_insert_cell(&draft, 1, txt("{\"a\":1}"), json!("{\"a\":1}"));
        e.stage_insert_cell(&draft, 3, None, Value::Null);

        let stmts = e.statements();
        assert_eq!(
            stmts[0].sql,
            "INSERT INTO \"main\".\"t\" (\"doc\", \"b\") VALUES (?::JSON, from_base64(?::VARCHAR)) RETURNING *"
        );
        assert_eq!(stmts[0].params, vec![json!("{\"a\":1}"), Value::Null]);
        assert_eq!(
            stmts[1].sql,
            "UPDATE \"main\".\"t\" SET \"doc\" = ?::JSON, \"j\" = ?::JSON, \"b\" = from_base64(?::VARCHAR) WHERE \"id\" = ?"
        );
        // The text goes as typed: the cast reads it, nothing re-serializes it.
        assert_eq!(stmts[1].params, vec![json!("{\"a\":1}"), json!("[1]"), json!("qrs="), json!(1)]);
    }

    #[test]
    fn a_blob_key_is_decoded_in_the_where_too() {
        let mut e = Edits::new(
            "\"main\".\"t\"".into(),
            vec!["k".into()],
            vec!["k".into(), "name".into()],
            vec!["BLOB".into(), "VARCHAR".into()],
        );
        e.stage_cell(vec![json!("AAE=")], 1, txt("a"), txt("b"), json!("b"));
        e.stage_delete(vec![json!("qg==")]);
        let stmts = e.statements();
        assert_eq!(
            stmts[0].sql,
            "UPDATE \"main\".\"t\" SET \"name\" = ? WHERE \"k\" = from_base64(?::VARCHAR)"
        );
        assert_eq!(stmts[1].sql, "DELETE FROM \"main\".\"t\" WHERE \"k\" = from_base64(?::VARCHAR)");
    }

    #[test]
    fn a_float_key_is_cast_in_the_where_and_bound_bare_as_a_value() {
        let mut e = Edits::new(
            "\"main\".\"t\"".into(),
            vec!["k".into(), "d".into()],
            vec!["k".into(), "d".into(), "f".into(), "name".into()],
            vec!["FLOAT".into(), "DOUBLE".into(), "FLOAT".into(), "VARCHAR".into()],
        );
        // The key itself is edited: the SET binds bare, the WHERE casts.
        e.stage_cell(vec![json!(1.1), json!(1.1)], 0, txt("1.1"), txt("2.2"), json!(2.2));
        e.stage_cell(vec![json!(1.1), json!(1.1)], 2, txt("0.1"), txt("0.2"), json!(0.2));
        e.stage_delete(vec![json!(0.1), json!(0.1)]);
        e.stage_duplicate(vec![json!(0.5), json!(0.5)], vec![(3, txt("a"), Bind::Source)]);
        let stmts = e.statements();
        let key = "WHERE \"k\" = ?::FLOAT AND \"d\" = ?";
        assert_eq!(
            stmts[0].sql,
            format!("INSERT INTO \"main\".\"t\" (\"name\") SELECT \"name\" FROM \"main\".\"t\" {key} RETURNING *")
        );
        assert_eq!(stmts[0].params, vec![json!(0.5), json!(0.5)]);
        assert_eq!(stmts[1].sql, format!("UPDATE \"main\".\"t\" SET \"k\" = ?, \"f\" = ? {key}"));
        assert_eq!(stmts[1].params, vec![json!(2.2), json!(0.2), json!(1.1), json!(1.1)]);
        assert_eq!(stmts[2].sql, format!("DELETE FROM \"main\".\"t\" {key}"));

        for ty in ["FLOAT", "float", "REAL", "FLOAT4"] {
            assert_eq!(key_placeholder_for(ty), "?::FLOAT", "{ty}");
        }
        // Only a FLOAT's own name: a DOUBLE round-trips the wire exactly, and
        // a container of FLOATs is no FLOAT.
        for ty in ["DOUBLE", "FLOAT[]", "STRUCT(f FLOAT)", "INTEGER", "VARCHAR", ""] {
            assert_eq!(key_placeholder_for(ty), "?", "{ty}");
        }
        assert_eq!(key_placeholder_for("BLOB"), "from_base64(?::VARCHAR)");
    }

    #[test]
    fn a_column_that_changed_type_is_a_different_shape() {
        let as_text = Edits::new(
            "\"main\".\"t\"".into(),
            vec!["id".into()],
            vec!["id".into(), "doc".into(), "j".into(), "b".into()],
            vec!["INTEGER".into(), "VARCHAR".into(), "JSON".into(), "BLOB".into()],
        );
        assert!(typed().same_shape(&typed()));
        assert!(!typed().same_shape(&as_text));
    }

    #[test]
    fn document_cells_take_json_text_and_keep_it_as_typed() {
        for ty in ["VARIANT", "JSON", "variant"] {
            for text in ["{\"a\": 1}", "[1, 2]", "\"Morel\"", "42", "12.340", "true", "18446744073709551616"] {
                assert_eq!(parse_value(text, ty), Ok(json!(text)), "{ty} {text}");
            }
            // The engine's JSON reads NaN and Infinity; JSON has neither, and
            // a document holding one reaches a client as a string.
            for text in [
                "Morel", "{oops", "[1, 2", "{'a': 1}", "\"NaN",
                "NaN", "-Infinity", "{\"x\": NaN, \"y\": [Infinity, -Infinity]}",
            ] {
                let err = parse_value(text, ty).unwrap_err();
                assert!(err.contains("is not JSON"), "{ty} {text}: {err}");
            }
            // Inside a string they are the string's own business.
            assert_eq!(parse_value("\"NaN and Infinity\"", ty), Ok(json!("\"NaN and Infinity\"")));

            // A document nests 100 levels and no deeper, and says so.
            let nested = |depth: usize, open: &str, close: &str| {
                format!("{}1{}", open.repeat(depth), close.repeat(depth))
            };
            for (open, close, levels) in [("[", "]", 1), ("{\"a\":", "}", 1), ("[{\"a\":", "}]", 2)] {
                let fits = nested(100 / levels, open, close);
                assert_eq!(parse_value(&fits, ty), Ok(json!(fits)), "{ty} {open}");
                let err = parse_value(&nested(100 / levels + 1, open, close), ty).unwrap_err();
                assert!(err.contains("nests deeper than 100 levels"), "{ty} {open}: {err}");
                assert!(!err.contains("is not JSON"), "{ty} {open}: {err}");
            }
            // Brackets inside a string open nothing, escaped quotes and all.
            let brackets = format!("[\"{} \\\" {}\"]", "[{".repeat(150), "[".repeat(150));
            assert_eq!(parse_value(&brackets, ty), Ok(json!(brackets)), "{ty}");
            // Siblings are not depth.
            let wide = format!("[{}]", vec!["[[1]]"; 200].join(","));
            assert_eq!(parse_value(&wide, ty), Ok(json!(wide)), "{ty}");
            // Unclosed text that deep is refused for its depth; shallower, as not JSON.
            assert!(parse_value(&"[".repeat(200), ty).unwrap_err().contains("nests deeper"));
            assert!(parse_value(&"[".repeat(50), ty).unwrap_err().contains("is not JSON"));
        }
        // `null` is SQL NULL in a VARIANT and a JSON value in a JSON column.
        assert_eq!(parse_value("null", "VARIANT"), Ok(Value::Null));
        assert_eq!(parse_value("null", "JSON"), Ok(json!("null")));
        // A BLOB cell is base64 text, decoded by its placeholder.
        assert_eq!(parse_value("qrs=", "BLOB"), Ok(json!("qrs=")));
    }

    #[test]
    fn inserts_omit_defaults_bind_values_and_undo_as_rows() {
        let mut e = edits();
        let defaults = e.stage_insert();
        let supplied = e.stage_insert();
        e.stage_insert_cell(&supplied, 1, txt("Ada"), json!("Ada"));
        e.stage_insert_cell(&supplied, 2, None, Value::Null);
        assert_eq!(e.counts(), (2, 0, 0));

        let stmts = e.statements();
        assert_eq!(stmts[0].sql, "INSERT INTO \"main\".\"t\" DEFAULT VALUES RETURNING *");
        assert!(stmts[0].params.is_empty());
        assert_eq!(
            stmts[1].sql,
            "INSERT INTO \"main\".\"t\" (\"name\", \"qty\") VALUES (?, ?) RETURNING *"
        );
        assert_eq!(stmts[1].params, vec![json!("Ada"), Value::Null]);
        assert_eq!(stmts[1].expectation, StatementExpectation::ReturnedOne);

        e.discard(&defaults);
        assert_eq!(e.counts(), (1, 0, 0));
        assert!(e.undo());
        assert_eq!(e.counts(), (2, 0, 0));
    }

    #[test]
    fn a_duplicate_is_one_insert_and_one_undo_step() {
        let mut e = edits();
        let key = e.stage_duplicate(
            vec![json!(5)],
            vec![(1, txt("Ada"), Bind::Source), (2, None, Bind::Source)],
        );
        assert_eq!(e.counts(), (1, 0, 0));
        assert_eq!(e.staged_text(&key, 1), Some(txt("Ada")));
        assert_eq!(e.staged_text(&key, 2), Some(None));
        assert!(e.undo());
        assert!(e.is_empty(), "the whole duplicate must undo at once");
        assert!(e.redo());
        assert_eq!(e.statements()[0].params, vec![json!(5)], "and comes back with its source");
    }

    #[test]
    fn a_duplicate_reads_untouched_cells_from_its_source_row() {
        let mut e = typed();
        // The source row carries a staged update on `j`, which the database
        // does not hold yet: that cell is bound, the others are read.
        let key = e.stage_duplicate(
            vec![json!(5)],
            vec![
                (1, txt("{\"when\":\"2024-02-29\"}"), Bind::Source),
                (2, txt("[2]"), Bind::Value(json!("[2]"))),
                (3, txt("qg=="), Bind::Source),
            ],
        );
        let stmts = e.statements();
        assert_eq!(
            stmts[0].sql,
            "INSERT INTO \"main\".\"t\" (\"doc\", \"j\", \"b\") \
             SELECT \"doc\", ?::JSON, \"b\" FROM \"main\".\"t\" WHERE \"id\" = ? RETURNING *"
        );
        assert_eq!(stmts[0].params, vec![json!("[2]"), json!(5)]);
        assert_eq!(stmts[0].expectation, StatementExpectation::ReturnedOne);

        // Typing into a copied cell makes it an ordinary typed value, and
        // the source keeps supplying the rest, through undo and redo.
        e.stage_insert_cell(&key, 3, txt("qrs="), json!("qrs="));
        let typed_over = e.statements();
        assert_eq!(
            typed_over[0].sql,
            "INSERT INTO \"main\".\"t\" (\"doc\", \"j\", \"b\") \
             SELECT \"doc\", ?::JSON, from_base64(?::VARCHAR) FROM \"main\".\"t\" WHERE \"id\" = ? RETURNING *"
        );
        assert_eq!(typed_over[0].params, vec![json!("[2]"), json!("qrs="), json!(5)]);
        assert!(e.undo());
        assert_eq!(e.statements(), stmts);
        assert!(e.redo());
        assert_eq!(e.statements(), typed_over);

        // With every copied cell typed over, nothing is read from the source.
        e.stage_insert_cell(&key, 1, None, Value::Null);
        assert_eq!(
            e.statements()[0].sql,
            "INSERT INTO \"main\".\"t\" (\"doc\", \"j\", \"b\") \
             VALUES (?::JSON, ?::JSON, from_base64(?::VARCHAR)) RETURNING *"
        );
        assert_eq!(e.statements()[0].params, vec![Value::Null, json!("[2]"), json!("qrs=")]);
    }

    #[test]
    fn a_copied_cell_typed_back_to_its_source_text_is_read_from_the_source_again() {
        let mut e = typed();
        let key = e.stage_duplicate(
            vec![json!(5)],
            vec![
                (1, txt("{\"when\":\"2024-02-29\"}"), Bind::Source),
                (2, txt("[2]"), Bind::Value(json!("[2]"))),
                (3, None, Bind::Source),
            ],
        );
        let copied = e.statements();
        assert_eq!(e.copied_text(&key, 1), Some(txt("{\"when\":\"2024-02-29\"}")));
        assert_eq!(e.copied_text(&key, 3), Some(None), "a copied NULL is remembered as one");
        assert_eq!(e.copied_text(&key, 2), None, "a bound cell copies nothing");
        assert_eq!(e.copied_text(&key, 0), None);

        // Typed over, the document is bound, and what it copied is kept.
        e.stage_insert_cell(&key, 1, txt("{}"), json!("{}"));
        assert_eq!(e.statements()[0].params, vec![json!("{}"), json!("[2]"), json!(5)]);
        assert_eq!(e.copied_text(&key, 1), Some(txt("{\"when\":\"2024-02-29\"}")));
        // Typed back, by value or by name, it is read from the source: the
        // DATE in it stays a DATE.
        e.stage_insert_cell(&key, 1, txt("{\"when\":\"2024-02-29\"}"), json!("{\"when\":\"2024-02-29\"}"));
        assert_eq!(e.statements(), copied);
        e.stage_insert_cell(&key, 1, txt("[]"), json!("[]"));
        e.stage_insert_copied(&key, 1);
        assert_eq!(e.statements(), copied);

        // Each of those was a step: undo walks back through them, redo forward.
        assert!(e.undo());
        assert_eq!(e.statements()[0].params, vec![json!("[]"), json!("[2]"), json!(5)]);
        assert!(e.undo());
        assert_eq!(e.statements(), copied);
        assert!(e.undo());
        assert_eq!(e.statements()[0].params, vec![json!("{}"), json!("[2]"), json!(5)]);
        assert!(e.undo());
        assert_eq!(e.statements(), copied);
        for _ in 0..4 {
            assert!(e.redo());
        }
        assert_eq!(e.statements(), copied);
        for _ in 0..4 {
            assert!(e.undo());
        }

        // A cell that already reads from the source is not staged again: the
        // whole duplicate is still one undo step.
        e.stage_insert_cell(&key, 3, None, Value::Null);
        e.stage_insert_copied(&key, 3);
        e.stage_insert_copied(&key, 1);
        e.stage_insert_copied(&key, 2);
        assert_eq!(e.statements(), copied);
        assert!(e.undo());
        assert!(e.is_empty(), "one ⌘Z removes the duplicate");

        // A cell bound from the start has no source text to return to, and
        // a draft that copies nothing has none at all.
        assert!(e.redo());
        e.stage_insert_cell(&key, 2, txt("[3]"), json!("[3]"));
        e.stage_insert_cell(&key, 2, txt("[2]"), json!("[2]"));
        assert_eq!(e.statements(), copied, "bound again, as it was");
        let fresh = e.stage_insert();
        e.stage_insert_cell(&fresh, 1, txt("1"), json!("1"));
        e.stage_insert_copied(&fresh, 1);
        assert_eq!(e.copied_text(&fresh, 1), None);
        assert_eq!(e.statements()[1].params, vec![json!("1")]);
    }

    #[test]
    fn a_duplicate_names_its_source_by_the_original_identity() {
        // The source row is both re-keyed and deleted in the same staged
        // set: the insert runs first and names the key the database holds.
        let mut e = edits();
        e.stage_cell(vec![json!(5)], 0, txt("5"), txt("7"), json!(7));
        e.stage_duplicate(vec![json!(5)], vec![(1, txt("Ada"), Bind::Source)]);
        e.stage_delete(vec![json!(9)]);
        e.stage_duplicate(vec![json!(9)], vec![(1, txt("Bo"), Bind::Source)]);
        let stmts = e.statements();
        let copy = "INSERT INTO \"main\".\"t\" (\"name\") SELECT \"name\" FROM \"main\".\"t\" WHERE \"id\" = ? RETURNING *";
        assert_eq!(stmts.len(), 4);
        assert_eq!((stmts[0].sql.as_str(), &stmts[0].params), (copy, &vec![json!(5)]));
        assert_eq!((stmts[1].sql.as_str(), &stmts[1].params), (copy, &vec![json!(9)]));
        assert!(stmts[2].sql.starts_with("UPDATE"));
        assert_eq!(stmts[2].params, vec![json!(7), json!(5)]);
        assert!(stmts[3].sql.starts_with("DELETE"));

        // A BLOB key is decoded in the duplicate's WHERE as in any other.
        let mut blob_keyed = Edits::new(
            "\"main\".\"t\"".into(),
            vec!["k".into()],
            vec!["k".into(), "name".into()],
            vec!["BLOB".into(), "VARCHAR".into()],
        );
        blob_keyed.stage_duplicate(vec![json!("AAE=")], vec![(1, txt("a"), Bind::Source)]);
        let stmts = blob_keyed.statements();
        assert_eq!(
            stmts[0].sql,
            "INSERT INTO \"main\".\"t\" (\"name\") SELECT \"name\" FROM \"main\".\"t\" \
             WHERE \"k\" = from_base64(?::VARCHAR) RETURNING *"
        );
        assert_eq!(stmts[0].params, vec![json!("AAE=")]);

        // A keyless table names the source by its hidden rowid, and a
        // composite key by every column of it.
        let mut keyless = Edits::new(
            "\"main\".\"t\"".into(),
            vec!["rowid".into()],
            vec!["rowid".into(), "name".into(), "doc".into()],
            vec!["BIGINT".into(), "VARCHAR".into(), "VARIANT".into()],
        );
        keyless.stage_duplicate(
            vec![json!(3)],
            vec![(1, txt("a"), Bind::Value(json!("b"))), (2, txt("1"), Bind::Source)],
        );
        let stmts = keyless.statements();
        assert_eq!(
            stmts[0].sql,
            "INSERT INTO \"main\".\"t\" (\"name\", \"doc\") SELECT ?, \"doc\" FROM \"main\".\"t\" \
             WHERE \"rowid\" = ? RETURNING *"
        );
        assert_eq!(stmts[0].params, vec![json!("b"), json!(3)]);

        let mut composite = Edits::new(
            "\"main\".\"t\"".into(),
            vec!["a".into(), "b".into()],
            vec!["a".into(), "b".into(), "name".into()],
            vec!["INTEGER".into(), "BLOB".into(), "VARCHAR".into()],
        );
        composite.stage_duplicate(vec![json!(1), json!("qg==")], vec![(2, txt("x"), Bind::Source)]);
        let stmts = composite.statements();
        assert_eq!(
            stmts[0].sql,
            "INSERT INTO \"main\".\"t\" (\"name\") SELECT \"name\" FROM \"main\".\"t\" \
             WHERE \"a\" = ? AND \"b\" = from_base64(?::VARCHAR) RETURNING *"
        );
        assert_eq!(stmts[0].params, vec![json!(1), json!("qg==")]);
    }

    #[test]
    fn a_duplicate_is_reviewed_discarded_and_validated_like_any_draft() {
        let mut e = edits();
        let key = e.stage_duplicate(vec![json!(5)], vec![(2, txt("3"), Bind::Source)]);
        // `name` is NOT NULL with no default and was not copied.
        let required = |e: &Edits| e.first_missing_required(&[true, true, false], &[None, None, None], &[true, false, false]);
        assert_eq!(required(&e), Some((key.clone(), 1)));
        e.stage_insert_cell(&key, 1, txt("Ada"), json!("Ada"));
        assert_eq!(required(&e), None, "a copied cell counts as supplied");
        let entries = e.entries();
        let RowChange::Insert(cells) = entries[0].2 else { panic!("an insert") };
        assert_eq!(cells[&2].text, txt("3"), "review shows the source's text");
        e.discard(&key);
        assert!(e.is_empty());
        assert!(e.undo());
        assert_eq!(e.statements()[0].params, vec![json!("Ada"), json!(5)]);
    }

    fn persisted<'a>(ty: &'a str, fetched: Option<&'a str>, staged: Option<Option<&'a str>>) -> Held<'a> {
        Held { ty, draft: false, fetched, staged, copied: None }
    }

    fn draft<'a>(ty: &'a str, staged: Option<Option<&'a str>>, copied: Option<Option<&'a str>>) -> Held<'a> {
        Held { ty, draft: true, fetched: None, staged, copied }
    }

    fn stage(text: &str, value: Value) -> Confirm {
        Confirm::Stage(txt(text), value)
    }

    #[test]
    fn confirming_a_persisted_cell_keeps_reverts_stages_or_refuses() {
        // An empty editor over NULL is NULL still — fetched or staged (⌃⇧N,
        // then Enter, Enter; or a Tab run through the cell), text or not.
        for ty in ["VARCHAR", "INTEGER", "VARIANT"] {
            assert_eq!(confirm("", &persisted(ty, None, None)), Confirm::Keep, "{ty}");
            assert_eq!(confirm("", &persisted(ty, Some("abc"), Some(None))), Confirm::Keep, "{ty}");
            assert_eq!(confirm("", &persisted(ty, Some(""), Some(None))), Confirm::Keep, "{ty}");
        }
        // And over '' it is '' still.
        assert_eq!(confirm("", &persisted("VARCHAR", Some(""), None)), Confirm::Keep);
        assert_eq!(confirm("", &persisted("VARCHAR", Some("abc"), Some(Some("")))), Confirm::Keep);
        assert_eq!(confirm("", &persisted("VARCHAR", None, Some(Some("")))), Confirm::Keep);

        // The text the cell holds, fetched or staged, is kept unjudged: none
        // of these is a value `parse_value` takes.
        for (ty, held) in [("VARIANT", "NaN"), ("INTEGER", "1e3"), ("VARIANT[]", "[1]"), ("DOUBLE", "x")] {
            assert_eq!(confirm(held, &persisted(ty, Some(held), None)), Confirm::Keep, "{ty}");
            assert_eq!(confirm(held, &persisted(ty, Some("0"), Some(Some(held)))), Confirm::Keep, "{ty}");
            assert_eq!(confirm(held, &persisted(ty, None, Some(Some(held)))), Confirm::Keep, "{ty}");
        }
        // Type-to-edit whose keystroke spells the value is the same confirm.
        assert_eq!(confirm("5", &persisted("INTEGER", Some("5"), None)), Confirm::Keep);

        // Staged text typed back to what was fetched reaches stage_cell, which
        // drops the edit; it is not judged either.
        assert_eq!(confirm("NaN", &persisted("VARIANT", Some("NaN"), Some(Some("1")))), Confirm::Revert);
        assert_eq!(confirm("NaN", &persisted("VARIANT", Some("NaN"), Some(None))), Confirm::Revert);
        assert_eq!(confirm("", &persisted("VARCHAR", Some(""), Some(Some("x")))), Confirm::Revert);
        // Emptied over a fetched NULL: NULL in, nothing typed, NULL out.
        assert_eq!(confirm("", &persisted("VARCHAR", None, Some(Some("x")))), Confirm::Revert);
        assert_eq!(confirm("", &persisted("INTEGER", None, Some(Some("7")))), Confirm::Revert);

        // An emptied cell that held something: '' for text, NULL otherwise.
        assert_eq!(confirm("", &persisted("VARCHAR", Some("abc"), None)), stage("", json!("")));
        assert_eq!(confirm("", &persisted("VARCHAR", Some("abc"), Some(Some("x")))), stage("", json!("")));
        assert_eq!(confirm("", &persisted("INTEGER", Some("7"), None)), Confirm::Stage(None, Value::Null));
        assert_eq!(confirm("", &persisted("VARCHAR[]", Some("[a]"), None)), Confirm::Stage(None, Value::Null));

        // Anything else is a typed value, judged by its type.
        assert_eq!(confirm("8", &persisted("INTEGER", Some("7"), None)), stage("8", json!(8)));
        assert_eq!(confirm("8", &persisted("INTEGER", None, None)), stage("8", json!(8)));
        assert_eq!(confirm("x", &persisted("VARCHAR", None, Some(None))), stage("x", json!("x")));
        assert_eq!(confirm("null", &persisted("INTEGER", Some("7"), None)), Confirm::Stage(None, Value::Null));
        assert_eq!(confirm("null", &persisted("VARCHAR", Some("7"), None)), stage("null", json!("null")));
        assert!(matches!(confirm("abc", &persisted("INTEGER", Some("7"), None)), Confirm::Refuse(_)));
        assert!(matches!(confirm("NaN", &persisted("VARIANT", Some("1"), None)), Confirm::Refuse(_)));
        assert!(matches!(confirm("NaN", &persisted("VARIANT", None, Some(Some("1")))), Confirm::Refuse(_)));
    }

    #[test]
    fn confirming_a_draft_cell_keeps_default_null_and_what_was_copied() {
        // Untouched, a draft cell stays DEFAULT: absent, not NULL, not ''.
        for ty in ["VARCHAR", "INTEGER"] {
            assert_eq!(confirm("", &draft(ty, None, None)), Confirm::Keep, "{ty}");
            // An explicit NULL stays the NULL it is, typed or copied, so a
            // Tab run through a duplicate adds no undo step.
            assert_eq!(confirm("", &draft(ty, Some(None), None)), Confirm::Keep, "{ty}");
            assert_eq!(confirm("", &draft(ty, Some(None), Some(None))), Confirm::Keep, "{ty}");
        }
        assert_eq!(confirm("", &draft("VARCHAR", Some(Some("")), None)), Confirm::Keep);
        assert_eq!(confirm("x", &draft("VARCHAR", None, None)), stage("x", json!("x")));
        assert!(matches!(confirm("x", &draft("INTEGER", None, None)), Confirm::Refuse(_)));
        assert_eq!(confirm("", &draft("VARCHAR", Some(Some("x")), None)), stage("", json!("")));
        assert_eq!(confirm("", &draft("INTEGER", Some(Some("7")), None)), Confirm::Stage(None, Value::Null));
        assert_eq!(confirm("7", &draft("INTEGER", Some(Some("7")), None)), Confirm::Keep);

        // A copied cell confirmed as it is: kept, unjudged. The digits are a
        // HUGEINT inside a VARIANT, the container one `parse_value` refuses.
        let big = "170141183460469231731687303715884105727";
        assert_eq!(confirm(big, &draft("VARIANT", Some(Some(big)), Some(Some(big)))), Confirm::Keep);
        assert_eq!(confirm("[1]", &draft("VARIANT[]", Some(Some("[1]")), Some(Some("[1]")))), Confirm::Keep);
        // Typed over and typed back, it reads from its source row again,
        // unjudged; so does a copied NULL emptied again, text column or not.
        assert_eq!(confirm(big, &draft("VARIANT", Some(Some("1")), Some(Some(big)))), Confirm::Revert);
        assert_eq!(confirm("NaN", &draft("VARIANT", Some(None), Some(Some("NaN")))), Confirm::Revert);
        assert_eq!(confirm("[1]", &draft("VARIANT[]", Some(None), Some(Some("[1]")))), Confirm::Revert);
        assert_eq!(confirm("", &draft("VARCHAR", Some(Some("x")), Some(None))), Confirm::Revert);
        assert_eq!(confirm("", &draft("VARCHAR", Some(Some("x")), Some(Some("")))), Confirm::Revert);
        // Typed to anything else, a copied cell is an ordinary typed cell.
        assert_eq!(confirm("2", &draft("VARIANT", Some(Some(big)), Some(Some(big)))), stage("2", json!("2")));
        assert!(matches!(confirm("[2]", &draft("VARIANT[]", Some(Some("[1]")), Some(Some("[1]")))), Confirm::Refuse(_)));
        // What a bound cell showed when it was duplicated is not a source text.
        assert!(matches!(confirm("NaN", &draft("VARIANT", Some(Some("1")), None)), Confirm::Refuse(_)));
    }

    #[test]
    fn a_container_of_documents_or_blobs_refuses_typed_text() {
        for (ty, inner) in [
            ("BLOB[]", "BLOB"),
            ("BLOB[2]", "BLOB"),
            ("VARIANT[]", "VARIANT"),
            ("JSON[]", "JSON"),
            ("json[]", "JSON"),
            ("STRUCT(v VARIANT, n INTEGER)", "VARIANT"),
            ("STRUCT(n INTEGER, \"B\" BLOB)", "BLOB"),
            ("STRUCT(json JSON)", "JSON"),
            ("MAP(VARCHAR, BLOB)", "BLOB"),
            ("MAP(BLOB, VARCHAR)", "BLOB"),
            ("UNION(n INTEGER, doc VARIANT)", "VARIANT"),
            ("STRUCT(a STRUCT(b DECIMAL(10,2), c JSON[])[])", "JSON"),
        ] {
            let err = parse_value("[]", ty).unwrap_err();
            assert!(err.contains("Query tab") && err.contains(&format!("the {inner} inside {ty}")), "{ty}: {err}");
            // NULL is a value of every one of them.
            assert_eq!(parse_value("null", ty), Ok(Value::Null), "{ty}");
            assert_eq!(confirm("", &persisted(ty, Some("[]"), None)), Confirm::Stage(None, Value::Null), "{ty}");
        }
        // A name is not a type: a field, an ENUM's value, a quoted identifier.
        for ty in [
            "STRUCT(json INTEGER, blob VARCHAR, variant DATE)",
            "UNION(blob INTEGER, json VARCHAR)",
            "STRUCT(\"JSON\" INTEGER, \"a \"\"BLOB\"\" b\" VARCHAR)",
            "ENUM('BLOB', 'JSON', 'it''s a VARIANT')",
            "STRUCT(a DECIMAL(10,2), json INTEGER)",
            "INTEGER[]",
            "MAP(VARCHAR, INTEGER)",
        ] {
            assert_eq!(document_or_blob_within(ty), None, "{ty}");
            assert_eq!(parse_value("x", ty), Ok(json!("x")), "{ty}");
        }
        // The three themselves are not containers of themselves.
        assert_eq!(parse_value("qrs=", "BLOB"), Ok(json!("qrs=")));
        assert_eq!(parse_value("[]", "VARIANT"), Ok(json!("[]")));
        assert_eq!(parse_value("[]", "JSON"), Ok(json!("[]")));
    }

    #[test]
    fn required_insert_validation_respects_defaults_and_generated_columns() {
        let mut e = edits();
        let key = e.stage_insert();
        assert_eq!(
            e.first_missing_required(
                &[true, true, true],
                &[Some("nextval('s')".into()), None, None],
                &[false, false, true],
            ),
            Some((key.clone(), 1))
        );
        e.stage_insert_cell(&key, 1, txt("Ada"), json!("Ada"));
        assert_eq!(
            e.first_missing_required(
                &[true, true, true],
                &[Some("nextval('s')".into()), None, None],
                &[false, false, true],
            ),
            None
        );
    }

    #[test]
    fn a_float_takes_a_number_a_float_can_hold() {
        for ty in ["FLOAT", "float", "REAL", "FLOAT4"] {
            // The largest FLOAT, and a DOUBLE that rounds to it.
            for text in ["3.4028235e38", "-3.4028235e38", "3.40282356e38", "1e-50", "0"] {
                assert!(parse_value(text, ty).is_ok(), "{ty} {text}");
            }
            // Halfway to the next power of two and beyond rounds past it.
            for text in ["3.4028235677973366e38", "3.4028236e38", "3.5e38", "-3.5e38", "1e39", "1e308"] {
                let err = parse_value(text, ty).unwrap_err();
                assert!(err.contains("out of range for") && err.contains(ty), "{ty} {text}: {err}");
            }
            // The names are not numbers and have no range.
            for name in ["nan", "inf", "-inf", "Infinity"] {
                assert_eq!(parse_value(name, ty), Ok(json!(name)), "{ty} {name}");
            }
        }
        for ty in ["DOUBLE", "FLOAT8"] {
            assert_eq!(parse_value("3.5e38", ty), Ok(json!(3.5e38)), "{ty}");
            assert_eq!(parse_value("1e308", ty), Ok(json!(1e308)), "{ty}");
            assert!(parse_value("1e309", ty).unwrap_err().contains("out of range"), "{ty}");
        }
        // A container of FLOATs is the engine's to judge.
        assert_eq!(parse_value("[1e39]", "FLOAT[]"), Ok(json!("[1e39]")));
    }

    #[test]
    fn text_typed_into_a_blob_cell_is_base64_and_never_null() {
        // Four base64 characters, three bytes: 9EE965, 3542CB, 36E965.
        for text in ["null", "NULL", "Null", "nUlL"] {
            assert_eq!(parse_value(text, "BLOB"), Ok(json!(text)), "{text}");
            assert_eq!(parse_value(text, "blob"), Ok(json!(text)), "{text}");
            assert_eq!(
                confirm(text, &persisted("BLOB", Some("qg=="), None)),
                stage(text, json!(text)),
                "{text}"
            );
            assert_eq!(confirm(text, &draft("BLOB", None, None)), stage(text, json!(text)), "{text}");
        }
        // A cell that shows those bytes is left as it is.
        assert_eq!(confirm("NULL", &persisted("BLOB", Some("NULL"), None)), Confirm::Keep);
        // An emptied BLOB cell is NULL, as Delete and ⌃⇧N make it.
        assert_eq!(confirm("", &persisted("BLOB", Some("qg=="), None)), Confirm::Stage(None, Value::Null));
        // Every other type that is not text keeps the rule, a container of
        // BLOBs among them.
        for ty in ["INTEGER", "DOUBLE", "DATE", "UUID", "VARIANT", "BLOB[]", "MAP(VARCHAR, BLOB)"] {
            assert_eq!(parse_value("NULL", ty), Ok(Value::Null), "{ty}");
        }
    }

    #[test]
    fn whitespace_around_a_value_that_is_not_text_is_not_a_change() {
        for (ty, held) in [
            ("INTEGER", "5"),
            ("DOUBLE", "1.5"),
            ("DECIMAL(10,2)", "1.50"),
            ("DATE", "2024-02-29"),
            ("BOOLEAN", "true"),
            ("UUID", "6f9619ff-8b86-d011-b42d-00c04fc964ff"),
            ("INTEGER[]", "[1, 2]"),
            ("VARIANT", "{\"a\":1}"),
            ("BLOB", "qg=="),
        ] {
            for typed in [format!(" {held}"), format!("{held} "), format!("\t{held} \n")] {
                // Over the fetched value, and over the same value staged.
                assert_eq!(confirm(&typed, &persisted(ty, Some(held), None)), Confirm::Keep, "{ty} {typed:?}");
                assert_eq!(
                    confirm(&typed, &persisted(ty, Some("0"), Some(Some(held)))),
                    Confirm::Keep,
                    "{ty} {typed:?}"
                );
                // Over another staged value it is the fetched one again.
                assert_eq!(
                    confirm(&typed, &persisted(ty, Some(held), Some(Some("0")))),
                    Confirm::Revert,
                    "{ty} {typed:?}"
                );
                assert_eq!(confirm(&typed, &persisted(ty, Some(held), Some(None))), Confirm::Revert, "{ty}");
                // A draft's cell, and what a duplicate copied.
                assert_eq!(confirm(&typed, &draft(ty, Some(Some(held)), None)), Confirm::Keep, "{ty}");
                assert_eq!(
                    confirm(&typed, &draft(ty, Some(Some("0")), Some(Some(held)))),
                    Confirm::Revert,
                    "{ty} {typed:?}"
                );
            }
        }
        // Staged with its padding, the bare value is that cell unchanged too.
        assert_eq!(confirm("6", &persisted("INTEGER", Some("5"), Some(Some(" 6")))), Confirm::Keep);
        // Spaces over NULL are NULL still; over a value they are judged.
        assert_eq!(confirm("  ", &persisted("INTEGER", None, None)), Confirm::Keep);
        assert!(matches!(confirm("  ", &persisted("INTEGER", Some("5"), None)), Confirm::Refuse(_)));
        // Whitespace inside the value is the value's own business.
        assert_eq!(
            confirm("[1,2]", &persisted("INTEGER[]", Some("[1, 2]"), None)),
            stage("[1,2]", json!("[1,2]"))
        );
        // A different value is staged as typed.
        assert_eq!(confirm(" 6", &persisted("INTEGER", Some("5"), None)), stage(" 6", json!(6)));

        // Where the padding is part of the value, the comparison is exact:
        // text, a JSON column's text, an ENUM's strings.
        assert_eq!(confirm(" 5", &persisted("VARCHAR", Some("5"), None)), stage(" 5", json!(" 5")));
        assert_eq!(confirm("5 ", &persisted("CHAR(3)", Some("5"), None)), stage("5 ", json!("5 ")));
        assert_eq!(confirm(" ", &persisted("VARCHAR", None, None)), stage(" ", json!(" ")));
        assert_eq!(confirm(" 5", &persisted("VARCHAR", Some(" 5"), Some(Some("5")))), Confirm::Revert);
        assert_eq!(confirm(" {}", &persisted("JSON", Some("{}"), None)), stage(" {}", json!(" {}")));
        assert_eq!(
            confirm(" a", &persisted("ENUM('a', ' a')", Some("a"), None)),
            stage(" a", json!(" a"))
        );
        assert_eq!(confirm(" 5", &draft("VARCHAR", Some(Some("5")), Some(Some("5")))), stage(" 5", json!(" 5")));
    }
}
