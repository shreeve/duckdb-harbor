//! Tab completion (lanes B and C).
//!
//! Lane B — the default: ask the server. `sql_auto_complete(?)` runs DuckDB's
//! own grammar-driven completer (PEG-backed on 2.0) against the live catalog,
//! through the same POST /sql everything else uses. Sub-ms over UDS.
//! Lane C — the fallback: a catalog cache plus the vendored keyword list,
//! used when the server misses the 150ms deadline, errors, or lacks
//! sql_auto_complete. Tab always answers.
//!
//! Setup (LOAD autocomplete, catalog fetch) is lazy — it runs on the first
//! Tab, not at connect, so a slow or extension-less berth never delays the
//! prompt. The cache lives behind an Arc so editor rebuilds (.keymode) share
//! it; .open swaps the connection and marks it stale.

use crate::keywords::KEYWORDS;
use crate::{Conn, http};
use wire::{Event, SqlRequest, endpoint};
use reedline::{Completer, CompletionResult, Span, Suggestion};
use std::io::BufRead;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone)]
pub struct SqlCompleter {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    conn: Conn,
    /// Table, column, and schema names from GET /catalog; None = not yet
    /// fetched (or stale after .open).
    catalog: Option<Vec<String>>,
}

impl SqlCompleter {
    pub fn new(conn: Conn) -> Self {
        Self { inner: Arc::new(Mutex::new(Inner { conn, catalog: None })) }
    }

    /// Point at a different berth (.open): drop the cache, refill lazily.
    pub fn reconnect(&self, conn: Conn) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.conn = conn;
        inner.catalog = None;
    }
}

impl Inner {
    fn setup(&mut self) {
        // Best-effort: sql_auto_complete lives in the autocomplete extension.
        // LOAD first (usually already installed); INSTALL+LOAD as a second
        // try. Failures are fine — lane C covers a berth without it.
        if quiet_sql(&self.conn, "LOAD autocomplete").is_none() {
            let _ = quiet_sql(&self.conn, "INSTALL autocomplete");
            let _ = quiet_sql(&self.conn, "LOAD autocomplete");
        }
        self.catalog = Some(fetch_catalog_names(&self.conn).unwrap_or_default());
    }

    fn server_suggestions(&self, prefix: &str, pos: usize) -> Option<Vec<Suggestion>> {
        let req = SqlRequest {
            sql: "SELECT suggestion, suggestion_start FROM sql_auto_complete(?)".into(),
            params: Some(vec![serde_json::Value::String(prefix.to_string())]),
            ..Default::default()
        };
        let body = serde_json::to_string(&req).ok()?;
        let resp = http::request(
            &self.conn.transport,
            &endpoint::SQL,
            self.conn.token.as_deref(),
            Some(&body),
            Some(Duration::from_millis(150)),
        )
        .ok()?;
        if resp.status >= 300 {
            return None; // no sql_auto_complete on this berth → lane C
        }
        let mut out = Vec::new();
        let mut reader = resp.body;
        let mut line = String::new();
        loop {
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => return None, // deadline hit mid-stream → lane C
            }
            match Event::parse(line.trim()) {
                Ok(Event::Row { values }) => {
                    let text = values.first().and_then(|v| v.as_str()).unwrap_or("");
                    let start = values.get(1).and_then(|v| v.as_u64()).unwrap_or(pos as u64);
                    if !text.is_empty() {
                        out.push(suggest(text.trim_end().to_string(), "", start as usize, pos));
                    }
                }
                Ok(Event::Error { .. }) => return None,
                _ => {}
            }
            line.clear();
        }
        Some(out)
    }

    fn cache_suggestions(&self, line: &str, pos: usize) -> Vec<Suggestion> {
        // Complete the word under the cursor from catalog names + keywords.
        let head = &line[..pos];
        let start = head
            .char_indices()
            .rev()
            .find(|&(_, c)| !c.is_alphanumeric() && c != '_')
            .map_or(0, |(i, c)| i + c.len_utf8());
        let word = &head[start..];
        if word.is_empty() {
            return Vec::new();
        }
        let wl = word.to_lowercase();
        let mut out: Vec<Suggestion> = Vec::new();
        for name in self.catalog.as_deref().unwrap_or_default() {
            if name.to_lowercase().starts_with(&wl) {
                out.push(suggest(name.clone(), "catalog", start, pos));
            }
        }
        let wu = word.to_uppercase();
        for kw in KEYWORDS {
            if kw.starts_with(&wu) {
                out.push(suggest((*kw).to_string(), "keyword", start, pos));
            }
        }
        out
    }
}

fn suggest(value: String, kind: &str, start: usize, end: usize) -> Suggestion {
    Suggestion {
        value,
        display_override: None,
        description: if kind.is_empty() { None } else { Some(kind.to_string()) },
        style: None,
        extra: None,
        span: Span { start, end },
        append_whitespace: false,
        match_indices: None,
    }
}

impl Completer for SqlCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> CompletionResult {
        // Clamped once, then used everywhere. `prefix` was clamped and
        // `cache_suggestions` was handed the raw value, which then sliced
        // `&line[..pos]` unguarded — if the clamp is worth having it is worth
        // having on both paths.
        let pos = pos.min(line.len());
        let prefix = &line[..pos];
        let list: Vec<Suggestion> = if prefix.trim_start().starts_with('.') {
            // Dot-commands are client-side (lane A), from the one list.
            crate::repl::DOT_COMMANDS
                .iter()
                .map(|(name, _, _)| format!(".{name}"))
                .filter(|c| c.starts_with(prefix.trim_start()))
                .map(|c| suggest(c, "command", 0, pos))
                .collect()
        } else {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if inner.catalog.is_none() {
                inner.setup(); // first Tab pays for it, the prompt never does
            }
            match inner.server_suggestions(prefix, pos) {
                Some(s) if !s.is_empty() => s,
                _ => inner.cache_suggestions(line, pos),
            }
        };
        CompletionResult::Fresh { suggestions: list.into(), partial: None }
    }
}

fn fetch_catalog_names(conn: &Conn) -> Option<Vec<String>> {
    let resp = http::request(
        &conn.transport,
        &endpoint::CATALOG,
        conn.token.as_deref(),
        None,
        Some(Duration::from_secs(2)),
    )
    .ok()?;
    if resp.status != 200 {
        return None;
    }
    let doc: serde_json::Value = serde_json::from_str(&resp.body_string().ok()?).ok()?;
    let mut names = Vec::new();
    if let Some(tables) = doc["tables"].as_array() {
        for t in tables {
            if let Some(n) = t["name"].as_str() {
                names.push(n.to_string());
            }
            if let Some(s) = t["schema"].as_str() {
                names.push(s.to_string());
            }
            if let Some(cols) = t["columns"].as_array() {
                for c in cols {
                    if let Some(n) = c["name"].as_str() {
                        names.push(n.to_string());
                    }
                }
            }
        }
    }
    names.sort();
    names.dedup();
    Some(names)
}

/// Run one statement, succeed quietly or return None. For setup steps whose
/// failure is acceptable.
fn quiet_sql(conn: &Conn, sql: &str) -> Option<()> {
    let body = serde_json::to_string(&SqlRequest { sql: sql.into(), ..Default::default() }).ok()?;
    let resp = http::request(
        &conn.transport,
        &endpoint::SQL,
        conn.token.as_deref(),
        Some(&body),
        Some(Duration::from_secs(5)),
    )
    .ok()?;
    if resp.status >= 300 {
        return None;
    }
    let mut reader = resp.body;
    let mut line = String::new();
    while let Ok(n) = reader.read_line(&mut line) {
        if n == 0 {
            break;
        }
        if matches!(Event::parse(line.trim()), Ok(Event::Error { .. })) {
            return None;
        }
        line.clear();
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Transport;

    #[test]
    fn word_boundary_is_char_safe() {
        // An em dash (or any multi-byte delimiter) before the word must not
        // slice mid-char — this input panicked the old byte-arithmetic.
        let inner = Inner {
            conn: Conn { transport: Transport::Tcp("127.0.0.1:1".into()), token: None },
            catalog: Some(vec!["people".into()]),
        };
        for line in ["select —peo", "select “peo", "select peo"] {
            let s = inner.cache_suggestions(line, line.len());
            assert!(s.iter().any(|s| s.value == "people"), "{line:?}");
        }
        // あ is a letter, so あpeo is one word — no match, and no panic
        assert!(inner.cache_suggestions("select あpeo", "select あpeo".len()).is_empty());
        assert!(inner.cache_suggestions("select あ", "select あ".len()).is_empty());
    }
}
