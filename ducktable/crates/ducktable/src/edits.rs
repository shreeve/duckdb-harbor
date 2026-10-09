//! The staging layer (docs/EDITING.md): every change the user has made
//! and not yet committed, keyed by row identity — the primary-key
//! columns' original fetched values, or the rowid-and-hash pair of a table
//! without a key — never by grid position. The view can sort, filter, and
//! page freely; nothing here moves.
//!
//! This module is pure model: no GPUI, no HTTP. The grid projects it
//! onto the current page for rendering; commit turns it into
//! parameterized statements. Every gesture — including a discard — is
//! one entry on the undo stack, so nothing is ever more than one
//! keystroke from recovery. A gesture that touches many rows (⌘⌫ over a
//! selection, discard-all) is one entry too: the rows stay separate
//! changes for review, and one ⌘Z takes the whole gesture back.

use gpui_kit::SharedString;
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
    /// Keyed by DuckDB's implicit rowid: the table has no key of its own.
    /// A rowid only names a position, and a checkpoint that compacts
    /// deleted rows renumbers the rest, so the one identity cell is the
    /// pair `[rowid, hash of the whole row]` (`sql::page_sql`) and every
    /// WHERE checks both. A row moved or changed since the fetch is named
    /// by nothing, and commit refuses instead of writing to its neighbor.
    by_rowid: bool,
    /// All schema column names, in result order (for SET clauses).
    columns: Vec<String>,
    /// Each column's DuckDB type, parallel to `columns`: what decides how
    /// its value is bound (`placeholder`).
    types: Vec<String>,
    changes: HashMap<String, Entry>,
    undo: Vec<Vec<Op>>,
    redo: Vec<Vec<Op>>,
    next_draft: u64,
    /// The COMMIT that sent this set got no answer, so the database may
    /// already hold it. While the doubt stands the set is held: nothing is
    /// staged into it or discarded from it, it yields no statements, and it
    /// has no undo history, since every step of that history predates a
    /// commit that may have landed. A commit is all or nothing, so the set
    /// leaves the hold whole, by one of two verdicts the user gives after
    /// comparing it with the database: it landed, and the set is dropped
    /// or it did not, and the set is staged again (`judge`). The flag stays
    /// with the set, which a table switch parks and hands back.
    in_doubt: bool,
    /// The session of that commit, while it has not been seen to end: the
    /// COMMIT may still be running there, and the database does not show its
    /// outcome yet.
    unsettled: Option<String>,
    /// No page has been read since that commit ended, so there is nothing
    /// to judge the set against. No verdict is taken while either stands;
    /// the next page read after the commit is over lifts both (`fetched`).
    unread: bool,
}

/// Why a held set cannot be judged yet.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Unjudged {
    /// The commit that sent it may still be running.
    Running,
    /// No page has been read since that commit ended.
    Unread,
}

/// What a grid does with the staged set parked for its table.
#[derive(Debug, PartialEq)]
pub enum Handoff {
    /// The grid's own set has the stash's shape: the stash takes its place.
    Adopt,
    /// The grid has no columns yet — its first fetch failed — so there is
    /// no shape to compare. The stash waits for the fetch that brings them.
    Hold,
    /// The table has another shape than the one the edits were staged
    /// against. Rebinding them would mis-key or mis-type them, so they are
    /// dropped.
    Orphan,
}

/// Decide a parked stash's fate. `mine` is the grid's own staging set, None
/// while it has none; `has_columns` is whether a page has reached the grid.
pub fn handoff(mine: Option<&Edits>, has_columns: bool, stash: &Edits) -> Handoff {
    match mine {
        Some(mine) if mine.same_shape(stash) => Handoff::Adopt,
        None if !has_columns => Handoff::Hold,
        _ => Handoff::Orphan,
    }
}

/// What a COMMIT's answer says of its transaction.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CommitOutcome {
    /// Harbor acknowledged it: everything since BEGIN is in the database.
    Landed,
    /// Nothing since BEGIN is in the database.
    NotLanded,
    /// The answer says nothing of the outcome: the server may have
    /// committed, or may be committing still.
    InDoubt,
}

/// Read the answer to a COMMIT sent on a session, `None` being success.
/// The grid's commit and the Query view judge it by this one rule.
///
/// Not landed: a COMMIT that could not be sent; one Harbor refused before
/// the engine saw it (the session gone, busy, or the server not serving),
/// which the release of the session rolls back; one the engine refused,
/// `400 sql_error`, Harbor's answer too for a COMMIT of a transaction an
/// earlier error aborted, which it rolls back and says so; and a `499
/// cancelled`, since a COMMIT runs to its answer and a cancel lands before
/// it starts or not at all (Harbor 0.44.2, the floor DuckTable requires).
/// In doubt: no answer, Harbor's `500 internal`, which it sends for a
/// statement the engine had already run, and any code this client does not
/// know, since nothing is assumed not to have run.
pub fn commit_outcome(failure: Option<&harbor_client::Failure>) -> CommitOutcome {
    use harbor_client::Failure;
    match failure {
        None => CommitOutcome::Landed,
        Some(Failure::Unsent(_)) => CommitOutcome::NotLanded,
        Some(Failure::Unanswered(_)) => CommitOutcome::InDoubt,
        Some(Failure::Refused { code, .. }) => match code.as_str() {
            "sql_error" | "cancelled" | "bad_request" | "not_found" | "forbidden" | "body_too_large"
            | "no_such_session" | "session_busy" | "query_id_in_use" | "no_lease_connections"
            | "no_lease_available" | "unavailable" | "unready" => CommitOutcome::NotLanded,
            _ => CommitOutcome::InDoubt,
        },
    }
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
            by_rowid: false,
            columns,
            types,
            changes: HashMap::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            next_draft: 1,
            in_doubt: false,
            unsettled: None,
            unread: false,
        }
    }

    /// Key this set by the rowid-and-hash pair a keyless page fetches.
    pub fn keyed_by_rowid(mut self) -> Self {
        self.pk_cols = vec!["rowid".to_string()];
        self.by_rowid = true;
        self
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
            && self.by_rowid == other.by_rowid
            && self.columns == other.columns
            && self.types == other.types
    }

    /// Whether anything is staged.
    pub fn any_staged(&self) -> bool {
        !self.changes.is_empty()
    }

    /// How many rows carry a staged change.
    pub fn len(&self) -> usize {
        self.changes.len()
    }

    /// Record that the COMMIT which sent this set got no answer. `unsettled`
    /// is the commit's session when it was not seen to end, and `read` is
    /// whether a page was read after it. The undo history goes with the
    /// certainty: a step back would restore a state from before a commit
    /// that may have landed.
    pub fn mark_in_doubt(&mut self, unsettled: Option<String>, read: bool) {
        if self.changes.is_empty() {
            return;
        }
        self.in_doubt = true;
        self.unread = !read || unsettled.is_some();
        self.unsettled = unsettled;
        self.undo.clear();
        self.redo.clear();
    }

    /// Whether the set is held because the commit that sent it got no answer.
    pub fn in_doubt(&self) -> bool {
        self.in_doubt
    }

    /// The session of the commit that sent the held set, while that commit
    /// may still be running.
    pub fn unsettled(&self) -> Option<&str> {
        self.unsettled.as_deref()
    }

    /// Why no verdict can be taken on the held set yet, if one cannot.
    pub fn unjudged(&self) -> Option<Unjudged> {
        match (self.in_doubt, &self.unsettled, self.unread) {
            (false, ..) => None,
            (true, Some(_), _) => Some(Unjudged::Running),
            (true, None, true) => Some(Unjudged::Unread),
            (true, None, false) => None,
        }
    }

    /// A page has been read. `over` is what was learned of the unsettled
    /// commit's session just before that read: `Some(true)`, it has ended.
    /// A page read after the commit is over is one the set can be judged
    /// against; one read while it may still be running is not.
    pub fn fetched(&mut self, over: Option<bool>) {
        if self.unsettled.is_some() && over != Some(true) {
            return;
        }
        self.unsettled = None;
        self.unread = false;
    }

    /// The user's verdict on the held set, taken whole: `landed`, and the
    /// set is dropped, history and all, so nothing of it is sent a second
    /// time; or not, and all of it is staged again, an ordinary staged set
    /// from here. Not taken, and false, while there is nothing to judge it
    /// against (`unjudged`).
    pub fn judge(&mut self, landed: bool) -> bool {
        if !self.in_doubt || self.unjudged().is_some() {
            return false;
        }
        if landed {
            self.clear();
        } else {
            self.in_doubt = false;
        }
        true
    }

    /// How the review popover names the row a duplicate copies: every key
    /// column with the value the WHERE binds, `copy of id = 5`, and `rowid`
    /// for a table keyed by it. None for a draft that copies nothing.
    pub fn source_label(&self, identity: &[Value]) -> Option<String> {
        if identity.is_empty() {
            return None;
        }
        if self.by_rowid {
            return Some(format!("copy of rowid = {}", self.bound_identity(identity).first()?));
        }
        let named = self
            .pk_cols
            .iter()
            .zip(identity)
            .map(|(col, value)| match value {
                Value::String(s) => format!("{col} = {s}"),
                other => format!("{col} = {other}"),
            })
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!("copy of {named}"))
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

    /// What the grid draws on its page: every staged change, or none while
    /// the set is held (`in_doubt`). A held set may have landed, and its
    /// identities were read before it did, so on the page fetched after the
    /// commit they can name other rows: a staged DELETE of id 7 would ghost
    /// the row a staged re-key of 3 to 7 made, and the re-key would show on
    /// no row at all. The page shows the database, and the set is reviewed
    /// in the popover, which lists `entries`.
    pub fn projection(&self) -> Vec<(&str, &[Value], &RowChange)> {
        if self.in_doubt { Vec::new() } else { self.entries() }
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
        // A held set takes no staging (`in_doubt`); `discard` is its one
        // mutation, and does not come through here.
        if self.in_doubt {
            return;
        }
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

    /// Stage one cell. Editing a value back to its original auto-cleans.
    /// A row staged for deletion takes no cell edit: the DELETE stands until
    /// it is undone or discarded, and a cell staged over it would replace it
    /// with an UPDATE. One entry per cell, last wins.
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
        if matches!(prev, Some(RowChange::Delete)) {
            return;
        }
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

    /// Whether `key` names a draft row nothing has been entered into: an
    /// INSERT with no cells, every column left to the database.
    pub fn is_untouched_insert(&self, key: &str) -> bool {
        matches!(
            self.changes.get(key).map(|e| &e.change),
            Some(RowChange::Insert(cells)) if cells.is_empty()
        )
    }

    /// Discard one row's staged change (the review popover's per-entry
    /// action). Itself undoable. A held set (`in_doubt`) gives up no single
    /// change: its commit was all or nothing, so either every change landed
    /// or none did, and one discarded alone would leave the rest to be sent
    /// against rows the others have already changed.
    pub fn discard(&mut self, key: &str) {
        if self.in_doubt {
            return;
        }
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
    /// transaction, and on the user's verdict that a held set's commit
    /// landed. The undo stack clears with it — commit is the line of no
    /// return, and the grammar says so out loud.
    pub fn clear(&mut self) {
        self.changes.clear();
        self.undo.clear();
        self.redo.clear();
        self.in_doubt = false;
        self.unsettled = None;
        self.unread = false;
    }

    /// The staged set as parameterized statements. Missing insert columns
    /// stay out of the statement so DuckDB supplies DEFAULT. The WHERE
    /// binds the ORIGINAL key values for existing rows.
    ///
    /// The order is what lets a set commit whatever keys it moves, since
    /// the engine checks a key as each statement runs. Duplicates come
    /// first: one with `Bind::Source` cells selects them from its source
    /// row, so the engine copies what the wire could not carry, and that
    /// row is read as the database holds it, whatever else is staged on it;
    /// a source row that is gone returns no row, which commit refuses. Then
    /// deletes, which free their keys; then updates, each after the one
    /// whose key it takes (`claim_order`); then new rows, which may take a
    /// key either of those freed. A delete of 7 and a re-key of 3 to 7 runs
    /// in that order, and so does a new row keyed 5 beside a delete of 5.
    ///
    /// A held set (`in_doubt`) yields none: it may already be in the
    /// database, and is not sent again until it has been staged again.
    pub fn statements(&self) -> Vec<Statement> {
        if self.in_doubt {
            return Vec::new();
        }
        // The table as the WHERE sees it: aliased when the row itself is
        // hashed, since `hash("t")` names a column if the table has one
        // called `t`, and an alias no column shares cannot.
        let (target, where_clause) = if self.by_rowid {
            let alias = qident(&self.row_alias());
            (
                format!("{} AS {alias}", self.source),
                format!("\"rowid\" = ? AND hash({alias}) = ?::UBIGINT"),
            )
        } else {
            let clause = self
                .pk_cols
                .iter()
                .map(|c| format!("{} = {}", qident(c), self.key_placeholder(c)))
                .collect::<Vec<_>>()
                .join(" AND ");
            (self.source.clone(), clause)
        };
        let (mut duplicates, mut deletes, mut updates, mut inserts) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        // Each update's key before and after, for `claim_order`.
        let mut moves = Vec::new();
        for (key, identity, change) in self.entries() {
            let mut params = Vec::new();
            match change {
                RowChange::Insert(cells) if cells.is_empty() => inserts.push(Statement {
                    sql: format!("INSERT INTO {} DEFAULT VALUES RETURNING *", self.source),
                    params,
                    expectation: StatementExpectation::ReturnedOne,
                }),
                RowChange::Insert(cells) => {
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
                    let copies = cells.values().any(|c| c.bind == Bind::Source);
                    let sql = if copies {
                        params.extend(self.bound_identity(identity));
                        format!(
                            "INSERT INTO {} ({names}) SELECT {supplied} FROM {target} WHERE {where_clause} RETURNING *",
                            self.source
                        )
                    } else {
                        format!("INSERT INTO {} ({names}) VALUES ({supplied}) RETURNING *", self.source)
                    };
                    let stmt = Statement { sql, params, expectation: StatementExpectation::ReturnedOne };
                    if copies { duplicates.push(stmt) } else { inserts.push(stmt) }
                }
                RowChange::Update(cells) => {
                    let set = cells
                        .iter()
                        .map(|(ix, cell)| {
                            format!("{} = {}", qident(self.column_name(*ix)), self.supply(*ix, cell, &mut params))
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    params.extend(self.bound_identity(identity));
                    moves.push((key.to_string(), self.moved_key(identity, cells)));
                    updates.push(Statement {
                        sql: format!("UPDATE {target} SET {set} WHERE {where_clause}"),
                        params,
                        expectation: StatementExpectation::AffectedOne,
                    });
                }
                RowChange::Delete => deletes.push(Statement {
                    sql: format!("DELETE FROM {target} WHERE {where_clause}"),
                    params: self.bound_identity(identity),
                    expectation: StatementExpectation::AffectedOne,
                }),
            }
        }
        let mut updates: Vec<Option<Statement>> = updates.into_iter().map(Some).collect();
        let updates = claim_order(&moves).into_iter().filter_map(|ix| updates[ix].take());
        duplicates.into_iter().chain(deletes).chain(updates).chain(inserts).collect()
    }

    /// The key an update gives its row, as `key_of` spells it, when the
    /// update changes a key column. A table keyed by rowid moves no key.
    fn moved_key(&self, identity: &[Value], cells: &BTreeMap<usize, CellEdit>) -> Option<String> {
        if self.by_rowid {
            return None;
        }
        let mut moved = identity.to_vec();
        for (slot, name) in moved.iter_mut().zip(&self.pk_cols) {
            let ix = self.columns.iter().position(|c| c == name);
            if let Some(Bind::Value(value)) = ix.and_then(|ix| cells.get(&ix)).map(|c| &c.bind) {
                *slot = value.clone();
            }
        }
        let moved = key_of(&moved);
        (moved != key_of(identity)).then_some(moved)
    }

    /// What a WHERE binds for `identity`: the key values, or the rowid and
    /// the row's hash unpacked from the pair a keyless page fetched.
    fn bound_identity(&self, identity: &[Value]) -> Vec<Value> {
        match identity {
            [Value::Array(pair)] if self.by_rowid => pair.clone(),
            _ => identity.to_vec(),
        }
    }

    /// An alias for the target that no column name shadows.
    fn row_alias(&self) -> String {
        let mut alias = "row".to_string();
        while self.columns.iter().any(|c| c.eq_ignore_ascii_case(&alias)) {
            alias.push('_');
        }
        alias
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

/// The order to run updates in, as indices into `moves`: each update's key
/// before, and after when it changes one. An update that takes the key
/// another leaves runs after that one, so a chain of re-keys (2 to 4, then 1
/// to 2) commits. Otherwise the updates keep their order. A cycle (a swap of
/// two keys) has no order that commits, and the engine refuses it.
fn claim_order(moves: &[(String, Option<String>)]) -> Vec<usize> {
    let leaves: HashMap<&str, usize> =
        moves.iter().enumerate().map(|(ix, (before, _))| (before.as_str(), ix)).collect();
    // The update each one waits on: the one whose key it takes.
    let waits_on: Vec<Option<usize>> = moves
        .iter()
        .enumerate()
        .map(|(ix, (_, after))| after.as_deref().and_then(|k| leaves.get(k)).copied().filter(|&on| on != ix))
        .collect();
    let mut placed = vec![false; moves.len()];
    let mut order = Vec::with_capacity(moves.len());
    for start in 0..moves.len() {
        // Walk back along what this update waits on, then run that chain
        // from its far end.
        let mut chain = Vec::new();
        let mut at = Some(start);
        while let Some(ix) = at.filter(|&ix| !placed[ix]) {
            placed[ix] = true;
            chain.push(ix);
            at = waits_on[ix];
        }
        order.extend(chain.into_iter().rev());
    }
    order
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

/// An ENUM's values, in declaration order: `ENUM('admin', 'user')` is
/// `admin`, `user`. A doubled quote is one quote, and a comma inside the
/// quotes belongs to the value. None for any other type, and for a
/// declaration that does not read as one.
pub(crate) fn enum_values(duck_type: &str) -> Option<Vec<String>> {
    let ty = duck_type.trim();
    let head = ty.get(..5)?;
    if !head.eq_ignore_ascii_case("ENUM(") || !ty.ends_with(')') {
        return None;
    }
    let mut values = Vec::new();
    let mut chars = ty[5..ty.len() - 1].chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        if chars.next()? != '\'' {
            return None;
        }
        let mut value = String::new();
        loop {
            match chars.next()? {
                '\'' if chars.next_if_eq(&'\'').is_some() => value.push('\''),
                '\'' => break,
                c => value.push(c),
            }
        }
        values.push(value);
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        match chars.next() {
            None => return Some(values),
            Some(',') => {}
            Some(_) => return None,
        }
    }
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

/// Whether a type is a number by its own name: an integer of any width
/// (HUGEINT and UHUGEINT among them), the unbounded BIGNUM, a float or a
/// decimal. A container of numbers, and a type whose spelling holds a
/// number's — INTERVAL, `ENUM('POINT')` — is not.
pub fn is_numeric_type(duck_type: &str) -> bool {
    let ty = duck_type.to_uppercase();
    let head = type_head(&ty);
    integer_bounds(head).is_some()
        || matches!(
            head,
            "BIGNUM" | "VARINT" | "DOUBLE" | "FLOAT8" | "FLOAT" | "FLOAT4" | "REAL" | "DECIMAL" | "NUMERIC"
        )
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
/// server's verdict at commit. `Value::Null` back means SQL NULL.
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
    // A UNION's text names no member. It is bound as a VARCHAR, and the
    // engine stores a VARCHAR under the member of that type: `8` typed over
    // `{"tag":"n","value":7}` in a `UNION(n INTEGER, s VARCHAR)` is stored as
    // the string '8' under `s`, and the displayed text typed back is stored
    // whole as a string. Without a VARCHAR member the cast fails at commit.
    if type_word_within(&ty, &["UNION"]).is_some() {
        return Err(format!(
            "typed text cannot say which member of {duck_type} it is \u{2014} edit this cell in the Query tab"
        ));
    }
    // A container's text is cast by the engine, and inside a quoted string
    // its cast reads a backslash as "take the next character as it is": `\"`,
    // `\'` and `\\` come back as the quotes and the backslash, and every
    // other escape loses its meaning. The cell shows a newline as `\n` and a tab
    // as `\t`, so the text of `["line\nbreak"]` typed back would store
    // `linenbreak`, with no error. Outside quotes the cast keeps a backslash
    // as typed; the rule is one rule all the same, since any value can be
    // written quoted.
    if is_container(&ty) {
        if let Some(escape) = lossy_escape(text) {
            return Err(format!(
                "typed text for {duck_type} can escape only a quote and a backslash: inside quotes the engine reads {escape} as plain characters \u{2014} edit this cell in the Query tab"
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
        // Bound as text so precision survives JSON; DuckDB casts, rounding
        // the digits past the scale. A number whose rounded value needs more
        // integer digits than the type has is refused here: at commit it
        // would fail the cast and take the whole transaction with it.
        let t = text.trim();
        let (width, scale) = decimal_shape(&ty);
        return match decimal_fits(t, width, scale) {
            Some(true) => Ok(Value::String(t.to_string())),
            Some(false) => Err(format!("{text:?} is out of range for {duck_type}")),
            None => Err(format!("{text:?} is not {duck_type}")),
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

/// A type whose text the engine casts element by element: a list or an
/// array (`INTEGER[]`, `VARCHAR[3]`), a STRUCT, a MAP. `ty` is uppercase.
fn is_container(ty: &str) -> bool {
    ty.ends_with(']') || matches!(type_head(ty), "STRUCT" | "MAP")
}

/// The first backslash escape in `text` that the engine's cast to a container
/// does not read back, inside quotes, as the character it names: any but
/// `\"`, `\'` and `\\`. A backslash that ends the text escapes nothing and
/// counts too.
fn lossy_escape(text: &str) -> Option<String> {
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            continue;
        }
        match chars.next() {
            Some('"') | Some('\'') | Some('\\') => {}
            Some(next) => return Some(format!("\\{next}")),
            None => return Some("\\".to_string()),
        }
    }
    None
}

/// A DECIMAL's width and scale from its type, `DECIMAL(18,3)` when it names
/// none, as the engine reads a bare `DECIMAL`. `ty` is uppercase.
fn decimal_shape(ty: &str) -> (u32, u32) {
    let inside = ty.find('(').and_then(|at| ty[at + 1..].strip_suffix(')'));
    let mut parts = inside.into_iter().flat_map(|p| p.split(',')).map(|p| p.trim().parse::<u32>());
    match (parts.next(), parts.next()) {
        (Some(Ok(width)), Some(Ok(scale))) => (width, scale),
        (Some(Ok(width)), None) => (width, 0),
        _ => (18, 3),
    }
}

/// Whether decimal text fits `DECIMAL(width, scale)` as the engine casts it:
/// digits past the scale are rounded, half away from zero, and what is left
/// may have `width - scale` integer digits. `999.994` fits `DECIMAL(5,2)` and
/// `999.995` does not, because it rounds to 1000.00. Before an exponent the
/// engine holds the digits to the same room, so `1000e-1` does not fit
/// where `100.0` does. None when the text is not a decimal number: digits
/// with at most one point, an optional sign and an optional exponent.
fn decimal_fits(text: &str, width: u32, scale: u32) -> Option<bool> {
    let unsigned = text.strip_prefix(['-', '+']).unwrap_or(text);
    let (mantissa, exponent) = match unsigned.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => {
            let digits = exponent.strip_prefix(['-', '+']).unwrap_or(exponent);
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            // An exponent too long to parse is far past any DECIMAL's 38
            // digits either way; the clamp below treats it as one.
            let size = digits.parse::<i64>().unwrap_or(i64::MAX).min(1_000);
            (mantissa, if exponent.starts_with('-') { -size } else { size })
        }
        None => (unsigned, 0),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole.is_empty() && fraction.is_empty()
        || !whole.bytes().chain(fraction.bytes()).all(|b| b.is_ascii_digit())
    {
        return None;
    }
    // The digits as one run, and where the point sits in it.
    let digits: Vec<u8> = whole.bytes().chain(fraction.bytes()).map(|b| b - b'0').collect();
    let point = whole.len() as i64 + exponent;
    // The digit `place` positions right of the point's left neighbor: 0 is
    // the ones digit, 1 the tens, -1 the first fractional digit.
    let digit_at = |place: i64| {
        usize::try_from(point - 1 - place).ok().and_then(|ix| digits.get(ix)).copied().unwrap_or(0)
    };
    let room = i64::from(width.saturating_sub(scale));
    let scale = i64::from(scale);
    if exponent != 0 && whole.trim_start_matches('0').len() as i64 > room {
        return Some(false);
    }
    // Any digit at or above 10^room is too many integer digits already.
    let leading = (point - room).clamp(0, digits.len() as i64) as usize;
    if digits[..leading].iter().any(|d| *d != 0) {
        return Some(false);
    }
    // Rounding carries past the last integer digit only when every kept
    // digit is a 9 and the first dropped one rounds up.
    let all_nines = (-scale..room).all(|place| digit_at(place) == 9);
    Some(!(all_nines && digit_at(-scale - 1) >= 5))
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
/// `STRUCT(v VARIANT)`, `MAP(VARCHAR, JSON)` — if it holds one.
fn document_or_blob_within(duck_type: &str) -> Option<&'static str> {
    type_word_within(duck_type, &["VARIANT", "JSON", "BLOB"])
}

/// The first of `wanted` that `duck_type` names as a type, itself or inside
/// it. A word of the type counts unless it is quoted (an ENUM's values, a
/// quoted field name) or is the field name that opens a STRUCT or UNION
/// member.
fn type_word_within(duck_type: &str, wanted: &[&'static str]) -> Option<&'static str> {
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
            } else if let Some(found) = wanted.iter().copied().find(|t| *t == word) {
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
    fn a_keyless_row_is_named_by_its_rowid_and_its_hash() {
        // A column called `row` shadows the alias, so the alias steps aside.
        let mut e = Edits::new(
            "\"main\".\"t\"".into(),
            vec![],
            vec!["rowid".into(), "row".into()],
            vec!["UBIGINT[]".into(), "VARCHAR".into()],
        )
        .keyed_by_rowid();
        e.stage_cell(vec![json!([5, 77])], 1, txt("a"), txt("b"), json!("b"));
        e.stage_delete(vec![json!([6, "18446744073709551615"])]);
        let stmts = e.statements();
        assert_eq!(
            stmts[0].sql,
            "DELETE FROM \"main\".\"t\" AS \"row_\" WHERE \"rowid\" = ? AND hash(\"row_\") = ?::UBIGINT"
        );
        assert_eq!(stmts[0].params, vec![json!(6), json!("18446744073709551615")]);
        assert_eq!(
            stmts[1].sql,
            "UPDATE \"main\".\"t\" AS \"row_\" SET \"row\" = ? WHERE \"rowid\" = ? AND hash(\"row_\") = ?::UBIGINT"
        );
        assert_eq!(stmts[1].params, vec![json!("b"), json!(5), json!(77)]);
    }

    #[test]
    fn statements_bind_original_identity_and_split_verbs() {
        let mut e = edits();
        e.stage_cell(vec![json!(5)], 0, txt("5"), txt("7"), json!(7));
        e.stage_cell(vec![json!(5)], 2, txt("1"), None, Value::Null);
        e.stage_delete(vec![json!(9)]);
        let stmts = e.statements();
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0].sql, "DELETE FROM \"main\".\"t\" WHERE \"id\" = ?");
        assert_eq!(stmts[0].params, vec![json!(9)]);
        assert_eq!(stmts[1].sql, "UPDATE \"main\".\"t\" SET \"id\" = ?, \"qty\" = ? WHERE \"id\" = ?");
        // A PK edit is just an update: SET binds the new value, WHERE the original.
        assert_eq!(stmts[1].params, vec![json!(7), Value::Null, json!(5)]);
    }

    /// Each statement's verb and the key its WHERE or VALUES names.
    fn plan(e: &Edits) -> Vec<(String, Value)> {
        e.statements()
            .into_iter()
            .map(|s| (s.sql.split(' ').next().unwrap().to_string(), s.params.last().cloned().unwrap_or(Value::Null)))
            .collect()
    }

    fn verb(name: &str, key: i64) -> (String, Value) {
        (name.to_string(), json!(key))
    }

    #[test]
    fn statements_free_a_key_before_another_row_takes_it() {
        // EDITING.md's example: a DELETE of 7 and a re-key of 3 to 7.
        let mut e = edits();
        e.stage_delete(vec![json!(7)]);
        e.stage_cell(vec![json!(3)], 0, txt("3"), txt("7"), json!(7));
        assert_eq!(plan(&e), vec![verb("DELETE", 7), verb("UPDATE", 3)]);

        // A new row keyed as a deleted one, and as one an update re-keys away.
        let mut e = edits();
        let draft = e.stage_insert();
        e.stage_insert_cell(&draft, 0, txt("5"), json!(5));
        let other = e.stage_insert();
        e.stage_insert_cell(&other, 0, txt("6"), json!(6));
        e.stage_delete(vec![json!(5)]);
        e.stage_cell(vec![json!(6)], 0, txt("6"), txt("60"), json!(60));
        assert_eq!(plan(&e), vec![verb("DELETE", 5), verb("UPDATE", 6), verb("INSERT", 5), verb("INSERT", 6)]);

        // A chain: 3 takes 4, 2 takes 3, 1 takes 2. Each runs after the one
        // whose key it takes, whatever order they were staged or sorted in.
        let mut e = edits();
        for (from, to) in [(1, 2), (2, 3), (3, 4)] {
            e.stage_cell(vec![json!(from)], 0, txt(&from.to_string()), txt(&to.to_string()), json!(to));
        }
        assert_eq!(plan(&e), vec![verb("UPDATE", 3), verb("UPDATE", 2), verb("UPDATE", 1)]);
        // A chain that ends in a deleted key: the delete comes first of all.
        e.stage_delete(vec![json!(4)]);
        assert_eq!(plan(&e)[0], verb("DELETE", 4));

        // A duplicate reads its source before the set deletes or re-keys it.
        let mut e = edits();
        e.stage_cell(vec![json!(5)], 0, txt("5"), txt("9"), json!(9));
        e.stage_delete(vec![json!(8)]);
        e.stage_duplicate(vec![json!(8)], vec![(1, txt("Ada"), Bind::Source)]);
        e.stage_duplicate(vec![json!(5)], vec![(1, txt("Bo"), Bind::Source)]);
        assert_eq!(
            plan(&e),
            vec![verb("INSERT", 8), verb("INSERT", 5), verb("DELETE", 8), verb("UPDATE", 5)]
        );

        // A composite key moves when any of its columns does.
        let mut e = Edits::new(
            "\"main\".\"t\"".into(),
            vec!["a".into(), "b".into()],
            vec!["a".into(), "b".into(), "name".into()],
            vec!["INTEGER".into(), "INTEGER".into(), "VARCHAR".into()],
        );
        e.stage_cell(vec![json!(1), json!(1)], 1, txt("1"), txt("2"), json!(2));
        e.stage_cell(vec![json!(1), json!(2)], 1, txt("2"), txt("3"), json!(3));
        let order: Vec<_> = e.statements().into_iter().map(|s| s.params).collect();
        assert_eq!(order, vec![vec![json!(3), json!(1), json!(2)], vec![json!(2), json!(1), json!(1)]]);
    }

    #[test]
    fn updates_run_after_the_ones_whose_keys_they_take() {
        let moves = |m: &[(&str, Option<&str>)]| -> Vec<(String, Option<String>)> {
            m.iter().map(|(a, b)| (a.to_string(), b.map(str::to_string))).collect()
        };
        // Nothing moves: the order stands.
        assert_eq!(claim_order(&moves(&[("1", None), ("2", None)])), vec![0, 1]);
        // 0 takes the key 1 leaves, and 1 the key 2 leaves.
        assert_eq!(claim_order(&moves(&[("1", Some("2")), ("2", Some("3")), ("3", Some("4"))])), vec![2, 1, 0]);
        // Two chains and a bystander keep their own places otherwise.
        let order = claim_order(&moves(&[("a", Some("b")), ("x", None), ("b", Some("c")), ("y", Some("z"))]));
        assert_eq!(order, vec![2, 0, 1, 3]);
        // A swap has no order that commits; each runs once all the same.
        let mut swap = claim_order(&moves(&[("1", Some("2")), ("2", Some("1"))]));
        swap.sort();
        assert_eq!(swap, vec![0, 1]);
        // A key moved to itself waits on nothing.
        assert_eq!(claim_order(&moves(&[("1", Some("1"))])), vec![0]);
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
    fn enum_values_read_the_declaration() {
        assert_eq!(
            enum_values("ENUM('admin', 'user')"),
            Some(vec!["admin".to_string(), "user".to_string()])
        );
        assert_eq!(
            enum_values("enum('it''s', 'a, b',' padded ')"),
            Some(vec!["it's".to_string(), "a, b".to_string(), " padded ".to_string()])
        );
        assert_eq!(enum_values("VARCHAR"), None);
        assert_eq!(enum_values("ENUM('open"), None);
        assert_eq!(enum_values("ENUM('a' 'b')"), None);
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
            "UPDATE \"main\".\"t\" SET \"doc\" = ?::JSON, \"j\" = ?::JSON, \"b\" = from_base64(?::VARCHAR) WHERE \"id\" = ?"
        );
        // The text goes as typed: the cast reads it, nothing re-serializes it.
        assert_eq!(stmts[0].params, vec![json!("{\"a\":1}"), json!("[1]"), json!("qrs="), json!(1)]);
        assert_eq!(
            stmts[1].sql,
            "INSERT INTO \"main\".\"t\" (\"doc\", \"b\") VALUES (?::JSON, from_base64(?::VARCHAR)) RETURNING *"
        );
        assert_eq!(stmts[1].params, vec![json!("{\"a\":1}"), Value::Null]);
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
        assert_eq!(stmts[0].sql, "DELETE FROM \"main\".\"t\" WHERE \"k\" = from_base64(?::VARCHAR)");
        assert_eq!(
            stmts[1].sql,
            "UPDATE \"main\".\"t\" SET \"name\" = ? WHERE \"k\" = from_base64(?::VARCHAR)"
        );
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
        assert_eq!(stmts[1].sql, format!("DELETE FROM \"main\".\"t\" {key}"));
        assert_eq!(stmts[2].sql, format!("UPDATE \"main\".\"t\" SET \"k\" = ?, \"f\" = ? {key}"));
        assert_eq!(stmts[2].params, vec![json!(2.2), json!(0.2), json!(1.1), json!(1.1)]);

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
    fn a_full_precision_double_key_names_its_row() {
        // 924.2100000029881 needs all seventeen digits. Read by serde_json's
        // default float parser it is its neighbor, 924.210000002988, and an
        // UPDATE keyed by that names no row.
        let line = r#"{"type":"row","values":[924.2100000029881,"a"]}"#;
        let wire::Event::Row { values } = wire::Event::parse(line).unwrap() else { panic!("a row") };
        let mut e = Edits::new(
            "\"main\".\"t\"".into(),
            vec!["k".into()],
            vec!["k".into(), "v".into()],
            vec!["DOUBLE".into(), "VARCHAR".into()],
        );
        e.stage_cell(vec![values[0].clone()], 1, txt("a"), txt("b"), json!("b"));
        let stmts = e.statements();
        assert_eq!(stmts[0].sql, "UPDATE \"main\".\"t\" SET \"v\" = ? WHERE \"k\" = ?");
        assert_eq!(serde_json::to_string(&stmts[0].params).unwrap(), r#"["b",924.2100000029881]"#);
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
        // The source rows are re-keyed and deleted in the same staged set:
        // the duplicates run first and name the keys the database holds.
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
        assert!(stmts[2].sql.starts_with("DELETE"));
        assert!(stmts[3].sql.starts_with("UPDATE"));
        assert_eq!(stmts[3].params, vec![json!(7), json!(5)]);

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

        // A keyless table names the source by its hidden rowid and the
        // row's hash, and a composite key by every column of it.
        let mut keyless = Edits::new(
            "\"main\".\"t\"".into(),
            vec![],
            vec!["rowid".into(), "name".into(), "doc".into()],
            vec!["UBIGINT[]".into(), "VARCHAR".into(), "VARIANT".into()],
        )
        .keyed_by_rowid();
        let pair = json!([3, "12016465711393625096"]);
        keyless.stage_duplicate(
            vec![pair.clone()],
            vec![(1, txt("a"), Bind::Value(json!("b"))), (2, txt("1"), Bind::Source)],
        );
        let stmts = keyless.statements();
        assert_eq!(
            stmts[0].sql,
            "INSERT INTO \"main\".\"t\" (\"name\", \"doc\") SELECT ?, \"doc\" FROM \"main\".\"t\" AS \"row\" \
             WHERE \"rowid\" = ? AND hash(\"row\") = ?::UBIGINT RETURNING *"
        );
        assert_eq!(stmts[0].params, vec![json!("b"), json!(3), json!("12016465711393625096")]);

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
    fn only_a_draft_with_nothing_entered_is_untouched() {
        let mut e = edits();
        let blank = e.stage_insert();
        assert!(e.is_untouched_insert(&blank));
        e.stage_insert_cell(&blank, 1, None, Value::Null);
        assert!(!e.is_untouched_insert(&blank), "an explicit NULL was entered");
        let copy = e.stage_duplicate(vec![json!(5)], vec![(2, txt("3"), Bind::Source)]);
        assert!(!e.is_untouched_insert(&copy), "a duplicate carries its copied cells");
        assert!(!e.is_untouched_insert("draft:missing"));
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
            "STRUCT(\"JSON\" INTEGER, \"a \"\"BLOB\"\" b\" VARCHAR)",
            "ENUM('BLOB', 'JSON', 'it''s a VARIANT')",
            "STRUCT(a DECIMAL(10,2), json INTEGER)",
            "INTEGER[]",
            "MAP(VARCHAR, INTEGER)",
        ] {
            assert_eq!(document_or_blob_within(ty), None, "{ty}");
            assert_eq!(parse_value("x", ty), Ok(json!("x")), "{ty}");
        }
        // A UNION's member names are names too; the UNION itself is refused
        // for its own reason (`a_union_refuses_typed_text`).
        assert_eq!(document_or_blob_within("UNION(blob INTEGER, json VARCHAR)"), None);
        // The three themselves are not containers of themselves.
        assert_eq!(parse_value("qrs=", "BLOB"), Ok(json!("qrs=")));
        assert_eq!(parse_value("[]", "VARIANT"), Ok(json!("[]")));
        assert_eq!(parse_value("[]", "JSON"), Ok(json!("[]")));
    }

    #[test]
    fn a_union_refuses_typed_text() {
        // Measured: `8` bound into `UNION(n INTEGER, s VARCHAR)` is stored
        // under `s` as the string '8', whichever member the cell showed.
        for ty in [
            "UNION(n INTEGER, s VARCHAR)",
            "union(n INTEGER, s VARCHAR)",
            "UNION(n INTEGER, d DATE)",
            "UNION(n INTEGER, s VARCHAR)[]",
            "STRUCT(u UNION(n INTEGER, s VARCHAR), k INTEGER)",
            "MAP(VARCHAR, UNION(a INTEGER, b DOUBLE))",
        ] {
            for text in ["8", "{\"tag\":\"n\",\"value\":8}", "x"] {
                let err = parse_value(text, ty).unwrap_err();
                assert!(err.contains("which member of") && err.contains(ty) && err.contains("Query tab"), "{ty}: {err}");
                assert!(matches!(confirm(text, &persisted(ty, Some("{\"tag\":\"n\",\"value\":7}"), None)), Confirm::Refuse(_)));
            }
            // NULL is a value of a UNION, and the text the cell holds is kept.
            assert_eq!(parse_value("null", ty), Ok(Value::Null), "{ty}");
            assert_eq!(confirm("", &persisted(ty, Some("x"), None)), Confirm::Stage(None, Value::Null), "{ty}");
            assert_eq!(confirm("x", &persisted(ty, Some("x"), None)), Confirm::Keep, "{ty}");
        }
        // A name is not a type: a field or an ENUM value called union.
        for ty in ["STRUCT(\"union\" INTEGER)", "STRUCT(union INTEGER)", "ENUM('UNION', 'x')"] {
            assert_eq!(parse_value("x", ty), Ok(json!("x")), "{ty}");
        }
    }

    #[test]
    fn a_container_refuses_an_escape_the_engine_does_not_read_back() {
        // Measured: `'["line\nbreak"]'::VARCHAR[]` is `linenbreak`, and
        // `{"b":"tab\there"}` as a STRUCT is `tabthere`; `\"` and `\\` are
        // the quote and the backslash. Quoted, `"C:\dir"` is `C:dir`.
        for ty in ["VARCHAR[]", "VARCHAR[2]", "STRUCT(b VARCHAR, n INTEGER)", "MAP(VARCHAR, VARCHAR)", "STRUCT(a VARCHAR[])[]"] {
            for (text, escape) in [
                (r#"["line\nbreak", "y"]"#, r"\n"),
                (r#"{"b":"tab\there"}"#, r"\t"),
                (r#"["a\rb"]"#, r"\r"),
                (r#"["a\u0001b"]"#, r"\u"),
                (r#"["ok\\", "then\b"]"#, r"\b"),
                (r#"["C:\dir"]"#, r"\d"),
                (r"[a\", "\\"),
            ] {
                let err = parse_value(text, ty).unwrap_err();
                assert!(
                    err.contains(&format!("the engine reads {escape} as plain characters"))
                        && err.contains(&format!("typed text for {ty} "))
                        && err.contains("Query tab"),
                    "{ty} {text}: {err}"
                );
                // A typed edit of a cell that holds one is refused; leaving it,
                // clearing it and NULL are not.
                assert!(matches!(confirm(text, &persisted(ty, Some("[]"), None)), Confirm::Refuse(_)), "{ty}");
                assert_eq!(confirm(text, &persisted(ty, Some(text), None)), Confirm::Keep, "{ty}");
                assert_eq!(confirm("", &persisted(ty, Some(text), None)), Confirm::Stage(None, Value::Null), "{ty}");
            }
            // The two escapes the cast reads back, and text with none.
            for text in [r#"["q\"uote"]"#, r#"["back\\slash"]"#, r#"["a\\\"b"]"#, r#"["plain", "it's"]"#, "[a, b]", r"['it\'s']", r#"["it\'s"]"#] {
                assert_eq!(parse_value(text, ty), Ok(json!(text)), "{ty} {text}");
            }
        }
        // Text is not cast element by element: a backslash in a VARCHAR is
        // a backslash, and a document goes through JSON, which reads `\n`.
        assert_eq!(parse_value(r"a\nb", "VARCHAR"), Ok(json!(r"a\nb")));
        assert_eq!(parse_value(r#"["a\nb"]"#, "VARIANT"), Ok(json!(r#"["a\nb"]"#)));
        assert_eq!(parse_value(r#"["a\nb"]"#, "JSON"), Ok(json!(r#"["a\nb"]"#)));
        assert_eq!(lossy_escape(r#"\\n \" \\\\"#), None);
        assert_eq!(lossy_escape(r"\\\n").as_deref(), Some(r"\n"));
    }

    #[test]
    fn a_decimal_is_held_to_its_width_and_rounded_to_its_scale() {
        // Measured against the engine for DECIMAL(5,2): digits past the scale
        // round half away from zero, and the rounded value has three integer
        // digits or the cast fails.
        for text in [
            "999.99", "-999.99", "999.994", "-999.994", "123.456", "0.005", "0.004", "-0.0001",
            "1e2", "12e0", "100e-2", "1e-3", ".5", "5.", "+1.5", " 12.5 ", "00012.5", "0", "999",
            "0.001e3", "9.99994e2", "0.1e3", "-1e2", "1E+02", "1e-1000", "999.99e0", "00999e-1",
            "0.0000000000000000000000000000000000000001",
        ] {
            assert_eq!(parse_value(text, "DECIMAL(5,2)"), Ok(json!(text.trim())), "{text}");
        }
        for text in [
            "12345.6", "1000", "-1000", "999.995", "-999.995", "999.999", "1e3", "100000e-2",
            "1e40", "1e999999999999999999999", "99999", "9.99999e2", "0.1e4", "-1e3",
            // The digits before an exponent are held to the integer room too.
            "99999e-2", "9999e-1", "1000e-1", "1234.5e-1",
        ] {
            let err = parse_value(text, "DECIMAL(5,2)").unwrap_err();
            assert!(err.contains("out of range for DECIMAL(5,2)"), "{text}: {err}");
        }
        for text in ["abc", "nan", "inf", "-", "+", ".", "1e", "e5", "1.2.3", "0x10", "--1", "1 2"] {
            let err = parse_value(text, "DECIMAL(5,2)").unwrap_err();
            assert!(err.contains("is not DECIMAL(5,2)"), "{text}: {err}");
        }
        // The editor is stricter than the engine in one spelling: the engine
        // reads an underscore between digits as a separator (measured,
        // `'1_0'::DECIMAL(5,2)` is 10.00), and the editor takes digits only.
        assert!(parse_value("1_0", "DECIMAL(5,2)").unwrap_err().contains("is not DECIMAL(5,2)"));
        // No integer digits at all, none past the point, and the widest type.
        assert!(parse_value("0.9999", "DECIMAL(4,4)").is_ok());
        assert!(parse_value("0.99994", "DECIMAL(4,4)").is_ok());
        assert!(parse_value("0.99995", "DECIMAL(4,4)").is_err());
        assert!(parse_value("1", "DECIMAL(4,4)").is_err());
        assert!(parse_value("999999999999999999", "DECIMAL(18,0)").is_ok());
        assert!(parse_value("999999999999999999.4", "DECIMAL(18,0)").is_ok());
        assert!(parse_value("999999999999999999.5", "DECIMAL(18,0)").is_err());
        assert!(parse_value("1000000000000000000", "DECIMAL(18,0)").is_err());
        assert!(parse_value(&"9".repeat(38), "DECIMAL(38,0)").is_ok());
        assert!(parse_value(&"9".repeat(39), "DECIMAL(38,0)").is_err());
        assert!(parse_value(&format!("{}.{}", "9".repeat(28), "9".repeat(10)), "DECIMAL(38,10)").is_ok());
        // NUMERIC is the same type, and a bare DECIMAL is DECIMAL(18,3).
        assert!(parse_value("12345.6", "NUMERIC(5,2)").is_err());
        assert_eq!(decimal_shape("DECIMAL"), (18, 3));
        assert_eq!(decimal_shape("DECIMAL(10)"), (10, 0));
        assert_eq!(decimal_shape("NUMERIC(38, 10)"), (38, 10));
        assert!(parse_value("999999999999999.9994", "DECIMAL").is_ok());
        assert!(parse_value("1000000000000000", "DECIMAL").is_err());
        // A container of DECIMALs is the engine's to judge.
        assert_eq!(parse_value("[12345.6]", "DECIMAL(5,2)[]"), Ok(json!("[12345.6]")));
    }

    #[test]
    fn a_set_whose_commit_got_no_answer_is_held_until_it_is_judged_whole() {
        let mut e = edits();
        // Nothing staged, nothing in doubt.
        e.mark_in_doubt(None, true);
        assert!(!e.in_doubt());

        let draft = e.stage_insert();
        e.stage_cell(vec![json!(1)], 1, txt("a"), txt("b"), json!("b"));
        e.stage_delete(vec![json!(7)]);
        assert_eq!(e.statements().len(), 3);
        e.mark_in_doubt(None, true);
        assert!(e.in_doubt());
        // Held, it is still the set, for review and for the count...
        assert_eq!(e.counts(), (1, 1, 1));
        assert_eq!(e.entries().len(), 3);
        // ...but it yields nothing to send, takes no staging, gives up no
        // single change, and has no history: every step of it predates a
        // commit that may have landed.
        assert!(e.statements().is_empty());
        e.stage_cell(vec![json!(2)], 1, txt("x"), txt("y"), json!("y"));
        e.stage_delete(vec![json!(3)]);
        e.stage_insert();
        e.discard(&draft);
        e.discard(&key_of(&[json!(7)]));
        e.grouped(|e| e.discard(&key_of(&[json!(1)])));
        assert_eq!(e.counts(), (1, 1, 1));
        assert!(!e.undo() && !e.redo());

        // The verdict that it did not land stages the whole set again.
        assert!(e.judge(false));
        assert!(!e.in_doubt());
        assert_eq!(e.statements().len(), 3);
        e.discard(&draft);
        assert!(e.undo(), "and its own gestures undo as ever");
        assert_eq!(e.counts(), (1, 1, 1));
        // A set that is not held takes no verdict.
        assert!(!e.judge(true));
        assert_eq!(e.counts(), (1, 1, 1));

        // The verdict that it landed drops the whole set, history and all.
        e.mark_in_doubt(None, true);
        assert!(e.judge(true));
        assert!(!e.in_doubt() && e.is_empty());
        assert!(!e.undo() && !e.redo(), "nothing comes back to be sent a second time");
    }

    #[test]
    fn a_held_set_takes_no_verdict_until_a_page_is_read_after_its_commit_is_over() {
        let mut e = edits();
        e.stage_insert();
        // The commit's session was not seen to end: the COMMIT may still be
        // running. Staged again now, a second transaction could insert the
        // row the first is still inserting.
        e.mark_in_doubt(Some("session-1".into()), true);
        assert_eq!((e.unsettled(), e.unjudged()), (Some("session-1"), Some(Unjudged::Running)));
        assert!(!e.judge(false) && !e.judge(true));
        assert!(e.in_doubt() && e.len() == 1 && e.statements().is_empty());
        // A page read while it may still be running changes nothing, whether
        // the session was found busy or could not be asked about.
        e.fetched(Some(false));
        e.fetched(None);
        assert_eq!(e.unjudged(), Some(Unjudged::Running));
        // A page read after it was seen to end is one to judge against.
        e.fetched(Some(true));
        assert_eq!((e.unsettled(), e.unjudged()), (None, None));
        assert!(e.judge(false));
        assert_eq!(e.statements().len(), 1);

        // The commit ended, and the page after it could not be read: the
        // page on screen is from before it.
        e.mark_in_doubt(None, false);
        assert_eq!(e.unjudged(), Some(Unjudged::Unread));
        assert!(!e.judge(true) && !e.judge(false));
        e.fetched(None);
        assert_eq!(e.unjudged(), None);
        assert!(e.judge(true));
        assert!(e.is_empty());

        // A commit that lands forgets all of it.
        e.stage_insert();
        e.mark_in_doubt(Some("session-2".into()), false);
        e.clear();
        assert_eq!((e.in_doubt(), e.unsettled(), e.unjudged()), (false, None, None));
    }

    #[test]
    fn a_held_set_is_reviewed_whole_and_not_drawn_on_the_page() {
        // A staged DELETE of id 7 and a re-key of 3 to 7. The commit lands
        // and its answer is lost: the row keyed 7 on the refetched page is
        // the one that was 3.
        let mut e = edits();
        e.stage_delete(vec![json!(7)]);
        e.stage_cell(vec![json!(3)], 0, txt("3"), txt("7"), json!(7));
        assert_eq!(e.projection().len(), 2);
        e.mark_in_doubt(None, true);
        // Nothing of it is drawn: no delete ghost on the row that is 7 at
        // present, no edit shown on a row 3 that is gone.
        assert!(e.projection().is_empty());
        // All of it is listed for review, and none of it can be sent.
        assert_eq!(e.entries().len(), 2);
        assert!(e.statements().is_empty());
        // The page shows a row 7 and no row 3: the re-key looks landed and
        // the DELETE does not. They landed or failed together, so the re-key
        // cannot be discarded alone, leaving the DELETE to remove the row it
        // made. The set stays whole.
        e.discard(&key_of(&[json!(3)]));
        assert_eq!(e.entries().len(), 2);
        assert!(e.in_doubt() && e.statements().is_empty() && e.projection().is_empty());
        // It landed: all of it is dropped, the DELETE with the re-key.
        assert!(e.judge(true));
        assert!(e.is_empty() && e.statements().is_empty());
    }

    #[test]
    fn a_number_is_known_by_its_own_type_name() {
        // Every numeric type name Harbor sends, measured from the engine, and
        // the aliases beside them.
        for ty in [
            "TINYINT", "SMALLINT", "INTEGER", "BIGINT", "HUGEINT", "UTINYINT", "USMALLINT",
            "UINTEGER", "UBIGINT", "UHUGEINT", "FLOAT", "DOUBLE", "DECIMAL(5,2)", "BIGNUM",
            "bigint", "VARINT", "REAL", "NUMERIC(5,0)",
        ] {
            assert!(is_numeric_type(ty), "{ty}");
        }
        for ty in ["INTERVAL", "INTEGER[]", "INTEGER[3]", "ENUM('POINT', 'INT')", "STRUCT(a INTEGER)", "VARCHAR", "POINT_2D", "DATE", ""] {
            assert!(!is_numeric_type(ty), "{ty}");
        }
    }

    #[test]
    fn a_row_staged_for_deletion_takes_no_cell_edit() {
        // ⌃⇧N, Delete and the editor all stage through `stage_cell`: on a
        // row staged for deletion none of them may swap the DELETE for an
        // UPDATE.
        let mut e = edits();
        let key = key_of(&[json!(1)]);
        e.stage_delete(vec![json!(1)]);
        e.stage_cell(vec![json!(1)], 2, txt("3"), None, Value::Null);
        e.stage_cell(vec![json!(1)], 1, txt("a"), txt("b"), json!("b"));
        assert!(e.is_deleted(&key));
        assert_eq!(e.counts(), (0, 0, 1));
        assert_eq!(e.statements().len(), 1);
        assert!(e.statements()[0].sql.starts_with("DELETE"));
        // The delete is still one undo step, and the row takes edits again
        // once it is back.
        assert!(e.undo());
        assert!(e.is_empty());
        e.stage_cell(vec![json!(1)], 2, txt("3"), None, Value::Null);
        assert_eq!(e.counts(), (0, 1, 0));
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

    #[test]
    fn the_review_names_the_row_a_duplicate_copies() {
        let e = edits();
        assert_eq!(e.source_label(&[json!(5)]).as_deref(), Some("copy of id = 5"));
        assert_eq!(e.source_label(&[json!("a b")]).as_deref(), Some("copy of id = a b"));
        // A draft that copies nothing has no source to name.
        assert_eq!(e.source_label(&[]), None);

        let composite = Edits::new(
            "\"main\".\"t\"".into(),
            vec!["a".into(), "b".into()],
            vec!["a".into(), "b".into(), "name".into()],
            vec!["INTEGER".into(), "BLOB".into(), "VARCHAR".into()],
        );
        assert_eq!(
            composite.source_label(&[json!(1), json!("qg==")]).as_deref(),
            Some("copy of a = 1, b = qg==")
        );
        let keyless = Edits::new(
            "\"main\".\"t\"".into(),
            vec![],
            vec!["rowid".into(), "name".into()],
            vec!["UBIGINT[]".into(), "VARCHAR".into()],
        )
        .keyed_by_rowid();
        assert_eq!(keyless.source_label(&[json!([3, 99])]).as_deref(), Some("copy of rowid = 3"));

        // The entries the popover walks carry exactly that identity.
        let mut e = edits();
        e.stage_insert();
        e.stage_duplicate(vec![json!(5)], vec![(1, txt("Ada"), Bind::Source)]);
        let labels: Vec<_> = e.entries().iter().map(|(_, identity, _)| e.source_label(identity)).collect();
        assert_eq!(labels, vec![None, Some("copy of id = 5".to_string())]);
    }

    #[test]
    fn a_commits_answer_says_it_landed_did_not_or_may_have() {
        use harbor_client::Failure;
        let refused = |code: &str| Failure::Refused { code: code.into(), message: "m".into() };
        assert_eq!(commit_outcome(None), CommitOutcome::Landed);
        // Never sent, refused before the engine saw it, refused by the
        // engine (a rolled-back COMMIT of an aborted transaction among
        // them), or cancelled before it started: nothing was kept.
        assert_eq!(commit_outcome(Some(&Failure::Unsent("refused".into()))), CommitOutcome::NotLanded);
        for code in [
            "sql_error", "cancelled", "no_such_session", "session_busy", "unavailable", "unready",
            "bad_request", "forbidden", "body_too_large", "not_found", "query_id_in_use",
            "no_lease_connections", "no_lease_available",
        ] {
            assert_eq!(commit_outcome(Some(&refused(code))), CommitOutcome::NotLanded, "{code}");
        }
        // No answer, an error after the engine ran it, or a code this client
        // does not know: it may have landed.
        assert_eq!(commit_outcome(Some(&Failure::Unanswered("timed out".into()))), CommitOutcome::InDoubt);
        for code in ["internal", "response_too_large", "some_later_code"] {
            assert_eq!(commit_outcome(Some(&refused(code))), CommitOutcome::InDoubt, "{code}");
        }
    }

    #[test]
    fn a_parked_stash_is_adopted_held_or_orphaned() {
        let mut stash = edits();
        stash.stage_cell(vec![json!(1)], 1, txt("a"), txt("b"), json!("b"));
        // The table as it was: the stash takes the grid's empty set's place.
        assert_eq!(handoff(Some(&edits()), true, &stash), Handoff::Adopt);
        // A grid whose first fetch failed has no columns and no set: the
        // stash waits, and is judged when they arrive.
        assert_eq!(handoff(None, false, &stash), Handoff::Hold);
        // A table that changed shape, or lost its key, orphans the stash.
        assert_eq!(handoff(Some(&typed()), true, &stash), Handoff::Orphan);
        assert_eq!(handoff(None, true, &stash), Handoff::Orphan);
        assert!(stash.any_staged() && !edits().any_staged());
    }
}
