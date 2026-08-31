//! The Query view (docs/QUERY.md): a berth-scoped SQL scratchpad above
//! a read-only results pane. The editor is gpui-component's code editor
//! with tree-sitter-duckdb highlighting; ⌘Enter sends the statement
//! under the caret; the scratch autosaves and survives restarts.
//!
//! v1 scaffold scope: marked-statement runs (no selection runs yet), a
//! single result per run, client-side truncation at the page size.

use crate::theme::{pal, value_font, CELL_TEXT, HEADER_TEXT};
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::table::{Column as TableColumn, Table, TableDelegate, TableState};
use gpui_component::{Sizable as _, StyledExt as _};
use harbor_client::Conn;

pub(crate) struct QueryView {
    conn: Conn,
    berth: String,
    pub(crate) editor: Entity<InputState>,
    results: Option<Entity<TableState<ResultsDelegate>>>,
    /// The last completed run's verdict, footer-rendered: (ms, rows,
    /// cols); cols == 0 is a resultless statement's "ok".
    stats: Option<(u64, u64, usize)>,
    /// A transient footer note ("nothing to run"), cleared by the next
    /// verdict.
    note: Option<SharedString>,
    error: Option<SharedString>,
    running: bool,
    /// Run generation: a stale result can never replace a newer one.
    generation: u64,
    /// When the in-flight run began — the elapsed clock's zero.
    run_started: Option<std::time::Instant>,
    /// True only after a run has held the floor for 300ms: fast queries
    /// swap atomically with no intermediate state at all; slow ones earn
    /// a ticking "running" line and faded prior results (Steve's
    /// three-phase ruling, 2026-08-31).
    show_running: bool,
    /// Set by the carousel landing here; consumed by the next render.
    needs_focus: bool,
    _subscription: Subscription,
    /// Keystroke interceptor for ⌘Enter: it must run BEFORE the input's
    /// own binding, which would insert a newline first (send means send,
    /// not send-and-type).
    _intercept: Subscription,
}

impl QueryView {
    pub(crate) fn new(
        conn: Conn,
        berth: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut state = InputState::new(window, cx)
                .code_editor("duckdb")
                .line_number(true)
                .placeholder("Type SQL. \u{2318}Enter runs the statement under the caret.");
            if let Some(text) = load_scratch(berth) {
                state = state.default_value(text);
            }
            state
        });
        // ⌘Enter arrives as the input's secondary-enter — the send key.
        // Plain Enter stays a newline (the editor's own default).
        let subscription = cx.subscribe_in(&editor, window, Self::on_editor_event);
        prune_history(berth);
        // The editor's Enter handler ALWAYS inserts a newline in
        // multi-line mode, secondary included, before emitting its
        // event. Interceptors run before binding dispatch, so this one
        // owns ⌘Enter outright while the editor is focused: run the
        // statement, stop the keystroke, buffer untouched. The
        // PressEnter subscription above stays as the backstop for any
        // path this guard doesn't cover.
        let weak = cx.entity().downgrade();
        let intercept = cx.intercept_keystrokes(move |ev, window, cx| {
            let m = &ev.keystroke.modifiers;
            if !(m.platform && !m.shift && !m.alt && !m.control)
                || ev.keystroke.key != "enter"
            {
                return;
            }
            let Some(view) = weak.upgrade() else { return };
            if !view.read(cx).editor.read(cx).focus_handle(cx).is_focused(window) {
                return;
            }
            view.update(cx, |view, cx| view.run(window, cx));
            cx.stop_propagation();
        });
        Self {
            conn,
            berth: berth.to_string(),
            editor,
            results: None,
            stats: None,
            note: None,
            error: None,
            running: false,
            generation: 0,
            run_started: None,
            show_running: false,
            needs_focus: false,
            _subscription: subscription,
            _intercept: intercept,
        }
    }

    /// True when this view already speaks for `berth` — table switches
    /// keep the scratchpad, reconnects rebuild it (docs/QUERY.md law 1).
    pub(crate) fn is_for(&self, berth: &str) -> bool {
        self.berth == berth
    }

    fn on_editor_event(
        &mut self,
        _: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::PressEnter { secondary: true } => self.run(window, cx),
            InputEvent::Change { .. } => {
                // Autosave rides the change event; a debounce can come
                // later — scratch writes are tiny.
                self.save_scratch(cx);
            }
            _ => {}
        }
    }

    fn save_scratch(&self, cx: &App) {
        let text = self.editor.read(cx).value().to_string();
        if let Some(path) = scratch_path(&self.berth) {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).ok();
            }
            std::fs::write(path, text).ok();
        }
    }

    /// ⌘Enter: send the statement under the caret (docs/QUERY.md). In
    /// the gaps between statements the one above owns the caret.
    pub(crate) fn run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.running {
            self.note = Some("already running\u{2026}".into());
            cx.notify();
            return;
        }
        let text = self.editor.read(cx).value().to_string();
        let caret = self.editor.read(cx).cursor();
        let Some(sql) = statement_at(&text, caret) else {
            self.note = Some("nothing to run".into());
            cx.notify();
            return;
        };
        self.running = true;
        self.generation += 1;
        let generation = self.generation;
        let conn = self.conn.clone();
        self.run_started = Some(std::time::Instant::now());
        // Phase 1: NOTHING on screen changes yet. A fast query (the
        // common case) replaces status and results in one frame when it
        // lands; only a run still going at 300ms earns visible chrome.
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(300))
                .await;
            // Phase 2: still running -> show the ticking line and fade
            // the prior results; tick ~100ms so the elapsed count moves.
            loop {
                let live = this
                    .update(cx, |this, cx| {
                        let live = this.running && this.generation == generation;
                        if live && !this.show_running {
                            this.show_running = true;
                        }
                        if live {
                            cx.notify();
                        }
                        live
                    })
                    .unwrap_or(false);
                if !live {
                    break;
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;
            }
        })
        .detach();
        cx.spawn_in(window, async move |this, cx| {
            let sql_logged = sql.clone();
            let outcome = cx
                .background_executor()
                .spawn(async move { harbor_client::query(&conn, &sql) })
                .await;
            this.update_in(cx, |this, window, cx| {
                // Fenced: only the newest run may land (docs/QUERY.md).
                if generation != this.generation {
                    return;
                }
                // Phase 3: one atomic swap — verdict line and results
                // land together, the fade lifts, nothing intermediate.
                this.running = false;
                this.show_running = false;
                this.run_started = None;
                this.error = None;
                this.note = None;
                append_history(&this.berth, &sql_logged, &outcome);
                match outcome {
                    Ok(result) => {
                        let rows = result.row_count;
                        let ms = result.time_ms;
                        let cols = result.columns.len();
                        this.stats = Some((ms, rows, cols));
                        if cols == 0 {
                            this.results = None;
                        } else {
                            let delegate = ResultsDelegate::new(result);
                            this.results = Some(cx.new(|cx| {
                                TableState::new(delegate, window, cx)
                                    .col_movable(false)
                            }));
                        }
                    }
                    Err(message) => {
                        this.error = Some(SharedString::from(message));
                        this.stats = None;
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The footer's status parts, in the Data view's exact voice and
    /// ordering (docs/QUERY.md): ticking while a slow run holds the
    /// floor, then the verdict — ms and rows leftmost, columns its own
    /// node (the anti-jump rule).
    pub(crate) fn status_parts(&self) -> (Option<String>, Option<String>) {
        if self.show_running {
            if let Some(t) = self.run_started {
                return (
                    Some(format!("running\u{2026} {} ms", t.elapsed().as_millis())),
                    None,
                );
            }
        }
        if let Some(note) = &self.note {
            return (Some(note.to_string()), None);
        }
        match self.stats {
            Some((ms, _, 0)) => (Some(format!("ok \u{00b7} {ms} ms")), None),
            Some((ms, rows, cols)) => (
                Some(format!("{ms} ms \u{00b7} {} rows", crate::util::commas(rows))),
                Some(format!("{cols} {}", if cols == 1 { "column" } else { "columns" })),
            ),
            None => (None, None),
        }
    }

    pub(crate) fn request_focus(&mut self, cx: &mut Context<Self>) {
        self.needs_focus = true;
        cx.notify();
    }
}

impl Render for QueryView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = pal(cx);
        if self.needs_focus {
            self.needs_focus = false;
            self.editor.read(cx).focus_handle(cx).focus(window);
        }
        div()
            .size_full()
            .min_h_0()
            .v_flex()
            .child(
                // The editor pane: the scratchpad, in the value font.
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    // The pane's one shared inset (theme::PANE_INSET), the
                    // same 12px the grid text and title strip align to.
                    // Horizontally the Input adds its own 8px (input_px,
                    // kept even with appearance off), so the wrapper
                    // supplies only the difference — 12 total, not 20.
                    .py(px(crate::theme::PANE_INSET))
                    .px(px(crate::theme::PANE_INSET - 8.))
                    .font_family(value_font())
                    // The same zoom ladder as the Data grid (Cmd-= / -),
                    // set ON the input: Input applies its own size-class
                    // text_sm before refining with caller styles, so a
                    // wrapper cascade never reaches the editor text.
                    .child(
                        Input::new(&self.editor)
                            .h_full()
                            // No border, no focus ring: the pane inset is
                            // the frame; the editor is just text.
                            .appearance(false)
                            .text_size(px(CELL_TEXT * crate::prefs::get(cx).zoom_factor())),
                    ),
            )
            .when_some(self.error.clone(), |d, message| {
                // The engine's verdict, verbatim (docs/QUERY.md law 4).
                d.child(
                    div()
                        .flex_none()
                        .px_3()
                        .py_2()
                        .border_t_1()
                        .border_color(t.border)
                        .text_xs()
                        .font_family(value_font())
                        .text_color(t.bad)
                        .child(message),
                )
            })
            // The run's verdict lives in the FOOTER's status line, the
            // same widgets and ordering as the Data view — no mid-pane
            // strip (Steve's unification ruling, 2026-08-31).
            .when_some(self.results.clone(), |d, table| {
                // The results pane: a snapshot that snaps (law 5). While
                // a slow run holds the floor, the prior snapshot fades —
                // visibly stale, never blanked.
                d.child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .border_t_1()
                        .border_color(t.border)
                        .when(self.show_running, |d| d.opacity(0.45))
                        .child(
                        Table::new(&table)
                            .bordered(false)
                            .with_size(crate::prefs::get(cx).table_size()),
                    ),
                )
            })
    }
}

// ============================ results table ===========================

/// A read-only results surface: names and display-ready texts, nothing
/// else. Editing powers stay with the Data grid's identity gate.
pub(crate) struct ResultsDelegate {
    cols: Vec<TableColumn>,
    rows: Vec<Vec<Option<SharedString>>>,
}

impl ResultsDelegate {
    fn new(result: harbor_client::QueryResult) -> Self {
        let cols = result
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| {
                TableColumn::new(
                    format!("c{i}"),
                    SharedString::from(c.name.clone().unwrap_or_default()),
                )
                .width(px(140.))
            })
            .collect();
        let rows = result
            .rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|v| match v {
                        serde_json::Value::Null => None,
                        serde_json::Value::String(s) => Some(SharedString::from(s)),
                        other => Some(SharedString::from(other.to_string())),
                    })
                    .collect()
            })
            .collect();
        Self { cols, rows }
    }
}

impl TableDelegate for ResultsDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.cols.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> &TableColumn {
        &self.cols[col_ix]
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let t = pal(cx);
        let p = crate::prefs::get(cx);
        let cell = self.rows.get(row_ix).and_then(|r| r.get(col_ix)).cloned().flatten();
        let base = div()
            .h_flex()
            .items_center()
            .h(p.table_size().table_row_height())
            .px_2()
            .text_size(px(CELL_TEXT * p.zoom_factor()))
            .font_family(value_font());
        match cell {
            Some(text) => base.text_color(t.text).child(text),
            None => base.text_color(t.muted).child("NULL"),
        }
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let t = pal(cx);
        let p = crate::prefs::get(cx);
        div()
            .px_2()
            .text_size(px(HEADER_TEXT * p.zoom_factor()))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(t.text)
            .child(self.cols[col_ix].name.clone())
    }
}

// =========================== statement spans ==========================

/// The statement owning `caret`: spans split on top-level semicolons,
/// aware of quotes, dollar-quotes, and both comment forms. In the gap
/// after a statement the one ABOVE owns the caret (docs/QUERY.md).
fn statement_at(text: &str, caret: usize) -> Option<String> {
    let spans = split_statements(text);
    let caret = caret.min(text.len());
    // The last statement that begins at or before the caret — so the
    // gap after a statement still belongs to it. Before the first
    // statement, the caret looks down.
    let pick = spans.iter().rposition(|s| s.start <= caret).unwrap_or(0);
    let sql = text[spans.get(pick)?.clone()].trim();
    (!sql.is_empty()).then(|| sql.to_string())
}

/// Byte ranges of `;`-separated statements, each excluding its
/// terminator. A tiny lexer, not a parser: it only needs to know what a
/// semicolon does NOT end — strings, quoted identifiers, comments, and
/// $$ bodies.
fn split_statements(text: &str) -> Vec<std::ops::Range<usize>> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' => {
                let quote = bytes[i];
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == quote {
                        // '' and "" are escapes, not terminators.
                        if bytes.get(i + 1) == Some(&quote) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 1;
            }
            b'$' if bytes.get(i + 1) == Some(&b'$') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'$' && bytes[i + 1] == b'$') {
                    i += 1;
                }
                i += 1;
            }
            b';' => {
                spans.push(start..i);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if start < bytes.len() {
        spans.push(start..bytes.len());
    }
    // Shrink each span to its trimmed content: the whitespace between
    // statements must not belong to the NEXT one (the gap rule above).
    spans
        .into_iter()
        .filter_map(|s| {
            let raw = &text[s.clone()];
            let lead = raw.len() - raw.trim_start().len();
            let tail = raw.len() - raw.trim_end().len();
            let (a, b) = (s.start + lead, s.end - tail);
            (a < b).then_some(a..b)
        })
        .collect()
}

fn scratch_path(berth: &str) -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let safe: String = berth
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    Some(
        std::path::Path::new(&home)
            .join(".config")
            .join("ducktable")
            .join("scratch")
            .join(format!("{safe}.sql")),
    )
}

fn load_scratch(berth: &str) -> Option<String> {
    std::fs::read_to_string(scratch_path(berth)?).ok()
}

fn history_path(berth: &str) -> Option<std::path::PathBuf> {
    let dir = scratch_path(berth)?;
    let name = dir.file_stem()?.to_string_lossy().to_string();
    Some(dir.parent()?.parent()?.join("history").join(format!("{name}.ndjson")))
}

/// One line per run, appended on completion (docs/QUERY.md: capture
/// before UI — history never captured is unrecoverable). NDJSON, not a
/// shell-style flat file: SQL is multi-line, and a run's verdict —
/// duration, rows, error — is what makes history a log of what
/// happened rather than a pile of text. The v2 recall popover reads
/// this; until then it is grep-food.
fn append_history(
    berth: &str,
    sql: &str,
    outcome: &Result<harbor_client::QueryResult, String>,
) {
    let Some(path) = history_path(berth) else { return };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let entry = match outcome {
        Ok(r) => serde_json::json!({
            "ts": ts, "sql": sql, "ok": true, "ms": r.time_ms, "rows": r.row_count,
        }),
        Err(message) => serde_json::json!({
            "ts": ts, "sql": sql, "ok": false, "error": message,
        }),
    };
    use std::io::Write as _;
    if let Ok(mut f) =
        std::fs::OpenOptions::new().create(true).append(true).open(&path)
    {
        writeln!(f, "{entry}").ok();
    }
}

/// The 10k-entry cap, enforced once per session at view birth — cheap,
/// bounded, and never on the run path.
fn prune_history(berth: &str) {
    let Some(path) = history_path(berth) else { return };
    let Ok(text) = std::fs::read_to_string(&path) else { return };
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() > 10_000 {
        let keep = &lines[lines.len() - 10_000..];
        std::fs::write(&path, format!("{}\n", keep.join("\n"))).ok();
    }
}

#[cfg(test)]
mod tests {
    // NOT `use super::*`: the render imports glob in gpui's `test`
    // attribute macro, which would shadow the built-in #[test] and
    // expand itself forever.
    use super::{split_statements, statement_at};

    #[test]
    fn splits_respect_quotes_and_comments() {
        let text = "SELECT 'a;b'; -- c;\nSELECT 2; /* ; */ SELECT 3";
        let spans = split_statements(text);
        let got: Vec<&str> = spans.iter().map(|s| text[s.clone()].trim()).collect();
        assert_eq!(got, ["SELECT 'a;b'", "-- c;\nSELECT 2", "/* ; */ SELECT 3"]);
    }

    #[test]
    fn caret_in_gap_belongs_to_statement_above() {
        let text = "SELECT 1;\n\nSELECT 2";
        // Caret just after the first semicolon, in the blank line.
        assert_eq!(statement_at(text, 10).as_deref(), Some("SELECT 1"));
        assert_eq!(statement_at(text, text.len()).as_deref(), Some("SELECT 2"));
        // Before anything: looks down.
        assert_eq!(statement_at("  SELECT 9", 0).as_deref(), Some("SELECT 9"));
    }
}
