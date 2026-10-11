//! The engine's own completer, `sql_auto_complete`, asked over `POST /sql`
//! on a pooled connection, never a session: a question about the text being
//! typed is no statement of the user's, and on a transaction's session an
//! error would abort the transaction.
//!
//! It lives in DuckDB's `autocomplete` extension. [`load`] asks the server to
//! load it, once; whether the server may fetch an extension it lacks is the
//! server's own setting, and a server that cannot load it is not asked to
//! complete.

use crate::fleet::Conn;
use crate::query::{exec_within, Failure, QueryResult};
use std::time::Duration;

/// One suggestion, as the engine ranks it.
#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    /// The text that replaces what is typed from `start`, without the
    /// space the engine adds after a keyword.
    pub text: String,
    /// The byte offset, in the text asked about, where the replacement
    /// begins.
    pub start: usize,
    /// What it names, in the engine's words: `keyword`, `table`, `column`,
    /// `scalar_function`, `table_function`, `schema`, `catalog`, …
    pub kind: String,
}

const ASK: &str = "SELECT suggestion, suggestion_start, suggestion_type FROM sql_auto_complete(?)";

/// Load the engine's completer on the server. A first load may download
/// the extension, so it waits up to `patience` for each read of the answer.
pub fn load(conn: &Conn, patience: Duration) -> Result<(), Failure> {
    exec_within(conn, "LOAD autocomplete", None, None, patience).map(|_| ())
}

/// The engine's suggestions for `sql`, a statement typed up to the caret,
/// waiting at most `patience` for each read of the answer.
pub fn suggest(conn: &Conn, sql: &str, patience: Duration) -> Result<Vec<Suggestion>, Failure> {
    let params = Some(vec![serde_json::Value::String(sql.to_string())]);
    exec_within(conn, ASK, params, None, patience).map(|answer| suggestions(&answer))
}

/// The rows of an answer, in the engine's order. A row without a text or a
/// start says nothing, and is passed over.
fn suggestions(answer: &QueryResult) -> Vec<Suggestion> {
    answer
        .rows
        .iter()
        .filter_map(|row| {
            let text = row.first()?.as_str()?.trim_end();
            let start = usize::try_from(row.get(1)?.as_u64()?).ok()?;
            let kind = row.get(2).and_then(|k| k.as_str()).unwrap_or_default();
            (!text.is_empty()).then(|| Suggestion { text: text.to_string(), start, kind: kind.to_string() })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{suggestions, QueryResult, Suggestion};
    use serde_json::json;

    #[test]
    fn a_suggestion_is_its_text_start_and_kind_without_the_trailing_space() {
        let answer = QueryResult {
            columns: Vec::new(),
            rows: vec![
                vec![json!("SELECT "), json!(0), json!("keyword")],
                vec![json!("\"Order Items\""), json!(14), json!("table")],
                // A row the engine could not fill says nothing.
                vec![json!(null), json!(3), json!("keyword")],
                vec![json!(" "), json!(3), json!("keyword")],
                vec![json!("x"), json!(-1), json!("keyword")],
                vec![json!("people"), json!(14)],
            ],
            row_count: 6,
            time_ms: 0,
        };
        let s = |text: &str, start, kind: &str| Suggestion { text: text.into(), start, kind: kind.into() };
        assert_eq!(
            suggestions(&answer),
            vec![s("SELECT", 0, "keyword"), s("\"Order Items\"", 14, "table"), s("people", 14, "")]
        );
    }
}
