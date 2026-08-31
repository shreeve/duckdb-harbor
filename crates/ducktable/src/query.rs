//! The Query view (docs/QUERY.md): a berth-scoped SQL scratchpad above
//! a read-only results pane. The editor is gpui-component's code editor
//! with tree-sitter-duckdb highlighting; ⌘Enter sends the statement
//! under the caret; the scratch autosaves and survives restarts.
//!
//! v1 scaffold scope: marked-statement runs (no selection runs yet), a
//! single result per run, client-side truncation at the page size.

use crate::theme::{pal, value_font, CELL_TEXT};
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::StyledExt as _;
use harbor_client::Conn;

pub(crate) struct QueryView {
    conn: Conn,
    berth: String,
    pub(crate) editor: Entity<InputState>,
    /// The results pane: a REAL Grid, embedded — "a Data window with a
    /// custom query preceding it". It pages, selects, and honors the
    /// display toggles exactly like the Data view, and it is read-only
    /// by construction (no catalog structure, no key, no Edits).
    results: Option<Entity<crate::grid::Grid>>,
    /// Bubbles the results grid's repaints (page flips, ticking loads)
    /// up to the app footer, which reads that grid through us.
    results_obs: Option<Subscription>,
    /// A resultless statement's verdict: the engine said ok in N ms.
    ok_ms: Option<u64>,
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
    /// The editor/results divider — user-draggable, position persisted
    /// (docs/QUERY.md's split, finally honored).
    split: Entity<gpui_component::resizable::ResizableState>,
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
        // The send mark (docs/QUERY.md law 3): what ⌘Enter will send
        // is visible before you press it. Any editor notify — a caret
        // move, an edit, even the cursor blink — recomputes the marked
        // rows; the guarded write means an unchanged mark costs
        // nothing.
        cx.observe(&editor, Self::sync_send_mark).detach();
        // The editor/results divider persists like every other divider
        // (sidebar, inspector): only the user's drag writes it.
        let split = cx.new(|_| gpui_component::resizable::ResizableState::default());
        cx.subscribe(
            &split,
            |_, state, _: &gpui_component::resizable::ResizablePanelEvent, cx| {
                if let Some(h) = state.read(cx).sizes().first().copied() {
                    crate::prefs::save(cx, |p| {
                        p.query_split = f32::from(h)
                            .clamp(crate::prefs::QUERY_SPLIT_MIN, crate::prefs::QUERY_SPLIT_MAX);
                    });
                }
            },
        )
        .detach();
        let mut this = Self {
            conn,
            berth: berth.to_string(),
            editor,
            results: None,
            results_obs: None,
            ok_ms: None,
            note: None,
            error: None,
            running: false,
            generation: 0,
            run_started: None,
            show_running: false,
            needs_focus: false,
            split,
            _subscription: subscription,
            _intercept: intercept,
        };
        // Seed the mark for the restored scratch before first paint.
        this.sync_send_mark(this.editor.clone(), cx);
        this
    }

    /// Recompute the send mark from the caret's statement and hand the
    /// editor's gutter its rows and color. The MEANING stays here,
    /// beside run(): both read statement_span, so the bar can never
    /// disagree with the payload. The same pass renumbers the gutter —
    /// line numbers restart at 1 on each statement's first line, so
    /// they match the engine's own "LINE n" in error messages — and
    /// pins the gutter to the grids' exact rail geometry, so the "1"
    /// never moves when ⌘1/2/3 switches views.
    fn sync_send_mark(&mut self, editor: Entity<InputState>, cx: &mut Context<Self>) {
        let (text, caret) = {
            let e = editor.read(cx);
            (e.value().to_string(), e.cursor())
        };
        // The mark's color is the header band's own background: the
        // marked statement's rail cells dim to it — what ⌘Enter will
        // send reads as a PLACE in the margin, not a sticker on it.
        let mark = statement_span(&text, caret).map(|r| {
            let start = text[..r.start].matches('\n').count();
            let end = text[..r.end].matches('\n').count();
            let shade = {
                use gpui_component::ActiveTheme as _;
                cx.theme().table_head
            };
            (start..end + 1, shade)
        });
        // Numbers live only on statement lines, restarting at 1 on
        // each statement — matching the engine's own "LINE n" — and
        // the gap rows between statements carry none (label 0 = silent
        // row). A blank line INSIDE a statement still counts: the
        // engine counts it too.
        let rows = text.matches('\n').count() + 1;
        let mut labels = vec![0u32; rows];
        let spans = split_statements(&text);
        // Only a `;` closes a band — ANY band, not just the last. The
        // open tail after the final `;` draws no closing hairline: the
        // line would claim "done here" under a mid-air thought. It
        // appears the moment the `;` does. end_rows lists the last row
        // of each statement that earned one.
        let mut end_rows: Vec<u32> = Vec::new();
        for span in spans {
            let closed = text[span.clone()].ends_with(';');
            let s = &text[span.clone()];
            let t_start = span.start + (s.len() - s.trim_start().len());
            let t_end = span.start + s.trim_end().len();
            if t_start >= t_end {
                continue;
            }
            let r0 = text[..t_start].matches('\n').count();
            let r1 = text[..t_end].matches('\n').count();
            for (i, r) in (r0..=r1).enumerate() {
                if labels[r] == 0 {
                    labels[r] = (i + 1) as u32;
                }
            }
            if closed {
                end_rows.push(r1 as u32);
            }
        }
        let max_label = labels.iter().copied().max().unwrap_or(1) as u64;
        // The rail obeys ⌥7 exactly like the grids: hidden means GONE
        // (the boundary line above the pane is all that remains).
        let show = crate::prefs::get(cx).row_numbers;
        editor.update(cx, |e, cx| e.set_line_number_visible(show, cx));
        // One rail for the whole pane: top and bottom both take the
        // wider of the editor's labels and the results' visible row
        // numbers — THIS pane's content, not the host table's (Steve's
        // content-fit ruling, 2026-08-31), with gutter_width's 2-digit
        // floor. Recompute from CURRENT content, rather than retaining
        // an old width, so a small result after a large one shrinks
        // both halves together.
        let results = self.results.clone();
        let results_last = results
            .as_ref()
            .map_or(0, |g| g.read(cx).last_visible_row(cx));
        let shared_max = shared_gutter_max(max_label, results_last);
        let rail = crate::grid::gutter_width(shared_max);
        if let Some(results) = results {
            results.update(cx, |grid, cx| grid.set_gutter_max(shared_max, cx));
        }
        // The render's ml(-8) walks the Input's own padding back, so
        // the text element — where the gutter begins — already sits at
        // the pane's left edge: the rail width is used verbatim.
        let t = crate::theme::pal(cx);
        let style = gpui_component::input::GutterStyle {
            width: px(rail),
            left_inset: px(0.),
            right_inset: px(6.),
            text_gap: px(12.),
            text_size: px(crate::theme::GUTTER_TEXT),
            background: t.raised,
            row_line: t.grid_line,
            border: t.border,
        };
        let stale = {
            let e = editor.read(cx);
            e.send_mark != mark
                || e.gutter_style.as_ref() != Some(&style)
                || e.gutter_end_rows.as_deref().map(Vec::as_slice)
                    != Some(end_rows.as_slice())
                || e.line_labels.as_deref().map(Vec::as_slice)
                    != Some(labels.as_slice())
        };
        if stale {
            editor.update(cx, |e, cx| {
                e.send_mark = mark;
                e.gutter_style = Some(style);
                e.gutter_end_rows = Some(std::rc::Rc::new(end_rows));
                e.line_labels = Some(std::rc::Rc::new(labels));
                cx.notify();
            });
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
        // Read on the UI thread; the fetch commits the size it ran with.
        let size = crate::prefs::get(cx).page_size;
        cx.spawn_in(window, async move |this, cx| {
            let sql_logged = sql.clone();
            // A send is at most the two queries the Data view gives a
            // table — and usually just ONE (Steve's probe-row ruling,
            // 2026-08-31): fetch page 0 of the wrapped statement with
            // LIMIT size+1. A result that fits the page IS its own
            // exact count — no second query. Only the extra row's
            // arrival proves there is more, and only then does
            // count(*) fire for the exact total. The page query
            // doubles as the wrap probe: if it fails (not actually
            // SELECT-shaped, or a syntax error), the statement runs
            // bare, so error verdicts always quote the user's own
            // SQL, never the wrapper's.
            let (outcome, total, paged) = cx
                .background_executor()
                .spawn(async move {
                    if wrappable(&sql) {
                        let src = crate::queries::query_source(&sql);
                        let probe = harbor_client::query(
                            &conn,
                            &crate::queries::page_sql(&src, false, &None, 0, size + 1),
                        );
                        if let Ok(mut result) = probe {
                            if result.rows.len() <= size {
                                let total = result.rows.len() as u64;
                                return (Ok(result), Some(total), true);
                            }
                            result.rows.truncate(size);
                            result.row_count = size as u64;
                            // A failed count leaves the total unknown;
                            // the grid's full-page heuristic still
                            // paces has_next, and the footer reads
                            // "1–5,000 rows" — honest, not wrong.
                            let total = harbor_client::query(
                                &conn,
                                &crate::queries::count_sql(&src, &None),
                            )
                            .ok()
                            .and_then(|r| crate::queries::count_of(&r));
                            return (Ok(result), total, true);
                        }
                    }
                    (harbor_client::query(&conn, &sql), None, false)
                })
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
                append_history(&this.berth, &sql_logged, &outcome, total);
                match outcome {
                    Ok(result) => {
                        let ms = result.time_ms;
                        if result.columns.is_empty() {
                            this.results = None;
                            this.results_obs = None;
                            this.ok_ms = Some(ms);
                        } else {
                            this.ok_ms = None;
                            // A paged run holds page 0 of a paged grid
                            // (total exact, or unknown if the count
                            // failed); a bare run (unwrappable, or the
                            // wrap probe failed) holds its entire
                            // result as one inert page whose total is
                            // its own length.
                            let (grid_total, page_size) = if paged {
                                (total, size)
                            } else {
                                (
                                    Some(result.rows.len() as u64),
                                    result.rows.len().max(1),
                                )
                            };
                            let conn = this.conn.clone();
                            let grid = cx.new(|cx| {
                                crate::grid::Grid::new_query(
                                    conn,
                                    &sql_logged,
                                    Ok(result),
                                    grid_total,
                                    page_size,
                                    paged,
                                    window,
                                    cx,
                                )
                            });
                            this.results_obs =
                                Some(cx.observe(&grid, |_, _, cx| cx.notify()));
                            this.results = Some(grid);
                        }
                    }
                    Err(message) => {
                        this.error = Some(SharedString::from(message));
                        this.results = None;
                        this.results_obs = None;
                        this.ok_ms = None;
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The footer's transient voice, which outranks the results grid's
    /// stats while it has something to say: the ticking elapsed line of
    /// a slow run, a note ("nothing to run"), or a resultless
    /// statement's "ok". The grid stats themselves come straight from
    /// the results grid — the footer reads it through results_grid().
    pub(crate) fn status_override(&self) -> Option<String> {
        if self.show_running {
            if let Some(t) = self.run_started {
                return Some(format!(
                    "running\u{2026} {}",
                    crate::util::human(t.elapsed().as_secs_f64(), "s")
                ));
            }
        }
        if let Some(note) = &self.note {
            return Some(note.to_string());
        }
        self.ok_ms
            .map(|ms| format!("ok \u{00b7} {}", crate::util::human(ms as f64 / 1000., "s")))
    }

    /// The embedded results grid, for the footer's stats and pager.
    pub(crate) fn results_grid(&self) -> Option<Entity<crate::grid::Grid>> {
        self.results.clone()
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
        // Results arriving can widen the shared rail; the guarded write
        // makes this free on every other frame.
        self.sync_send_mark(self.editor.clone(), cx);
        div()
            .size_full()
            .min_h_0()
            .v_flex()
            .child({
                // The grids' header row, echoed: same height, same
                // paint, a "#" on the number rail — so ⌘1/2/3 keeps
                // the chrome still and only the content changes.
                let row_h = crate::prefs::get(cx).table_size().table_row_height();
                // The rail's visible width: the style's width minus the
                // Input-padding walk-back (left_inset), so "#" lands on
                // the numbers' right edge exactly like the grids'.
                let gw = self
                    .editor
                    .read(cx)
                    .gutter_style
                    .as_ref()
                    .map(|g| g.width - g.left_inset)
                    .unwrap_or(px(crate::grid::gutter_width(1)));
                div()
                    .flex_none()
                    .w_full()
                    .h(row_h)
                    .h_flex()
                    .items_center()
                    // No bottom border of its own: the editor paints
                    // the boundary at its viewport's fixed top — one
                    // line, never two, and scrolling can't carry it
                    // away (it would collide with the -0.5px overlap
                    // here and render half-weight).
                    .bg({
                        use gpui_component::ActiveTheme as _;
                        cx.theme().table_head
                    })
                    // The "#" cell is the rail's header: with the rail
                    // hidden (⌥7 off) it goes too, exactly like the
                    // grids' # column.
                    .when(crate::prefs::get(cx).row_numbers, |d| d.child(
                        // The "#" cell wears the rail's right edge,
                        // exactly like the grids' # header — the
                        // vertical hairline runs unbroken from band
                        // to footer.
                        div()
                            .w(gw)
                            .h_full()
                            .h_flex()
                            .items_center()
                            .border_r_1()
                            .border_color(t.border)
                            .child(
                                div()
                                    .w_full()
                                    .text_right()
                                    .pr(px(6.))
                                    .text_size(px(crate::theme::GUTTER_TEXT))
                                    .font_family(value_font())
                                    .text_color(t.muted)
                                    .child("#"),
                            ),
                    ))
            })
            .map(|d| {
                // The editor pane: the scratchpad, in the value font.
                // Flush at the pane's left so the imposed gutter
                // (sync_send_mark) sits exactly on the grids' number
                // rail; the Input's built-in 8px is walked back by the
                // negative margin. Line height is the grids' row
                // height, so line 1 sits where row 1 sits.
                let editor_pane = div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    // Half a logical pixel up: the grids center row
                    // content in row_h minus the 1px bottom border,
                    // the editor in the full line box — this is the
                    // difference, measured on screen. No bottom
                    // padding: the rail runs all the way to the
                    // footer.
                    .mt(px(-0.5))
                    .font_family(value_font())
                    // The same zoom ladder as the Data grid (Cmd-= / -),
                    // set ON the input: Input applies its own size-class
                    // text_sm before refining with caller styles, so a
                    // wrapper cascade never reaches the editor text.
                    .child(
                        Input::new(&self.editor)
                            .h_full()
                            // Kill the Input's own padding outright
                            // (caller styles refine): the rail starts
                            // at the pane's true left edge, hairlines
                            // reach its true right edge, and the rail
                            // runs to the footer — no dead strips on
                            // any side. The gutter and the editor's
                            // internal margins supply all the space.
                            .p_0()
                            // No border, no focus ring: the pane inset is
                            // the frame; the editor is just text.
                            .appearance(false)
                            .text_size(px(CELL_TEXT * crate::prefs::get(cx).zoom_factor()))
                            .line_height(px(f32::from(
                                crate::prefs::get(cx).table_size().table_row_height(),
                            ))),
                    );
                // The engine's verdict, verbatim (docs/QUERY.md law 4)
                // — it rides the editor's pane, above the divider.
                let err_strip = self.error.clone().map(|message| {
                    div()
                        .flex_none()
                        .px_3()
                        .py_2()
                        .border_t_1()
                        .border_color(t.border)
                        .text_xs()
                        .font_family(value_font())
                        .text_color(t.bad)
                        .child(message)
                });
                // The run's verdict lives in the FOOTER's status line,
                // the same widgets and ordering as the Data view — no
                // mid-pane strip (Steve's unification ruling,
                // 2026-08-31).
                match self.results.clone() {
                    // The results pane: a snapshot that snaps (law 5).
                    // While a slow run holds the floor, the prior
                    // snapshot fades — visibly stale, never blanked.
                    // The divider between the panes is the user's: a
                    // draggable 1px splitter whose position persists
                    // (docs/QUERY.md's split), the handle's own line
                    // standing in for the old border_t.
                    Some(grid) => d.child(
                        gpui_component::resizable::v_resizable("query-split")
                            .with_state(&self.split)
                            .child(
                                gpui_component::resizable::resizable_panel()
                                    .size(px(crate::prefs::get(cx).query_split))
                                    .size_range(
                                        px(crate::prefs::QUERY_SPLIT_MIN)
                                            ..px(crate::prefs::QUERY_SPLIT_MAX),
                                    )
                                    // Furniture: only the user's drag
                                    // moves the divider — a window
                                    // resize gives its delta to the
                                    // results.
                                    .fixed()
                                    .child(
                                        div()
                                            .size_full()
                                            .min_h_0()
                                            .v_flex()
                                            .child(editor_pane)
                                            .children(err_strip),
                                    ),
                            )
                            .child(
                                gpui_component::resizable::resizable_panel().child(
                                    div()
                                        .size_full()
                                        .min_h_0()
                                        .when(self.show_running, |d| d.opacity(0.45))
                                        .child(grid),
                                ),
                            ),
                    ),
                    None => d.child(editor_pane).children(err_strip),
                }
            })
    }
}

/// Statements the results grid can page by wrapping in a subquery —
/// DuckDB accepts `SELECT * FROM (statement) LIMIT …` for the
/// SELECT-shaped family. Anything else stays a single inert page.
fn wrappable(sql: &str) -> bool {
    let mut s = sql.trim_start();
    while let Some(rest) = s.strip_prefix("--") {
        s = rest.split_once('\n').map(|(_, r)| r).unwrap_or("").trim_start();
    }
    if s.starts_with('(') {
        return true;
    }
    let word: String =
        s.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    matches!(
        word.to_ascii_lowercase().as_str(),
        "select" | "with" | "from" | "values" | "table"
    )
}

// =========================== statement spans ==========================

/// The statement owning `caret`, as a byte range trimmed to its actual
/// text — INCLUDING its terminating `;`, which belongs to the
/// statement: spans split on top-level semicolons, aware of quotes,
/// dollar-quotes, and both comment forms. In the gap after a statement
/// the one ABOVE owns the caret (docs/QUERY.md). This range is what
/// ⌘Enter sends (minus the terminator — statement_at) AND what the
/// send mark spans — one function, so the bar can never lie about the
/// payload.
fn statement_span(text: &str, caret: usize) -> Option<std::ops::Range<usize>> {
    let spans = split_statements(text);
    let caret = caret.min(text.len());
    // The last statement that begins at or before the caret — so the
    // gap after a statement still belongs to it. Before the first
    // statement, the caret looks down.
    let pick = spans.iter().rposition(|s| s.start <= caret).unwrap_or(0);
    let span = spans.get(pick)?.clone();
    let s = &text[span.clone()];
    let start = span.start + (s.len() - s.trim_start().len());
    let end = span.start + s.trim_end().len();
    (start < end).then(|| start..end)
}

/// The payload: the statement WITHOUT its terminator. The span owns
/// the `;` (it's part of the statement's place — band, labels, mark),
/// but the wire never needs it and the pager's subquery wrap
/// (`SELECT * FROM (…) LIMIT n`) can't syntactically hold one.
fn statement_at(text: &str, caret: usize) -> Option<String> {
    statement_span(text, caret)
        .map(|r| {
            let s = &text[r];
            // Shed exactly ONE terminator — a span owns one at most,
            // and anything deeper (`-- note;`) is statement text.
            s.strip_suffix(';').map_or(s, str::trim_end).to_string()
        })
        .filter(|s| !s.is_empty())
}

/// The editor and its embedded results grid are one vertical pane, so
/// their row-number rails have one width derived from current content.
fn shared_gutter_max(top: u64, bottom: u64) -> u64 {
    top.max(bottom)
}

/// Byte ranges of statements, each excluding its terminator. A tiny
/// lexer, not a parser: it only needs to know what a boundary does NOT
/// end — strings, quoted identifiers, comments, and $$ bodies.
///
/// ONE boundary exists (the semicolon ruling, 2026-08-31): a top-level
/// `;` — the same authority DuckDB's own parser answers to, and the
/// same mark that closes a statement's band in the gutter. Blank lines
/// never divide: DuckDB's FROM-first syntax makes every keyword
/// heuristic lie eventually (`from 22` IS a statement), and a wrong
/// split can leave a runnable prefix — `delete from orders` above a
/// pondered `where` clause must never become sendable on its own. A
/// wrong merge, by contrast, is a loud syntax error. So scribble
/// freely; the `;` says "done", splits the thought, and closes its
/// band in one keystroke.
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
                // The terminator BELONGS to its statement (Steve's
                // ruling): a `;` on its own line is the statement's
                // last row, not a stray gap row outside the band.
                spans.push(start..i + 1);
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
    // A span that is nothing but terminators (a stray `;;`) is no
    // statement at all.
    spans
        .into_iter()
        .filter_map(|s| {
            let raw = &text[s.clone()];
            let lead = raw.len() - raw.trim_start().len();
            let tail = raw.len() - raw.trim_end().len();
            let (a, b) = (s.start + lead, s.end - tail);
            (a < b && !text[a..b].chars().all(|c| c == ';')).then_some(a..b)
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
    // A counted run's exact total — history should say the query
    // MATCHED 520k rows, not that page 0 held 5,000 of them.
    total: Option<u64>,
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
            "ts": ts, "sql": sql, "ok": true, "ms": r.time_ms,
            "rows": total.unwrap_or(r.row_count),
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
    use super::{shared_gutter_max, split_statements, statement_at, statement_span};

    #[test]
    fn semicolons_are_the_only_divider() {
        let split = |t: &str| {
            split_statements(t)
                .iter()
                .map(|s| t[s.clone()].trim().to_string())
                .collect::<Vec<_>>()
        };
        // The semicolon ruling: blank lines never divide — DuckDB's
        // FROM-first syntax means `from 22` opens a real statement, so
        // any keyword heuristic must eventually cut a sprawled query
        // in half. Only a `;` divides.
        assert_eq!(
            split("select\n\n1\n\nfrom\n\n22;"),
            ["select\n\n1\n\nfrom\n\n22;"]
        );
        // The landmine that motivated the caution: a pondered WHERE
        // below a gap must never leave a runnable DELETE prefix.
        assert_eq!(
            split("delete from orders\n\nwhere x < 1;"),
            ["delete from orders\n\nwhere x < 1;"]
        );
        // A terminated scribble above an open one: the `;` divides,
        // the gap after it belongs to nobody.
        assert_eq!(split("select 1;\n\nselect"), ["select 1;", "select"]);
        // Everyday napkin flow: one `;` per thought, gaps at will.
        assert_eq!(
            split("select count(*) from t;\n\n-- next\nselect 2;"),
            ["select count(*) from t;", "-- next\nselect 2;"]
        );
        // The terminator BELONGS to its statement — a `;` on its own
        // line is the statement's last row, not a stray scrap — and a
        // span of nothing but `;` is no statement at all.
        assert_eq!(split("select 1\n;"), ["select 1\n;"]);
        assert_eq!(split("select 1;;"), ["select 1;"]);
    }

    #[test]
    fn query_rails_share_the_current_maximum() {
        assert_eq!(shared_gutter_max(6, 98_765), 98_765);
        assert_eq!(shared_gutter_max(6, 20), 20);
        assert_eq!(shared_gutter_max(123, 20), 123);
    }

    #[test]
    fn send_mark_spans_hug_the_statement() {
        // The span is trimmed to the statement's actual text, so the
        // gutter bar hugs its lines — no leading blank-line slack from
        // the raw semicolon-to-semicolon split.
        let text = "SELECT 1;\n\nSELECT\n  2;\n";
        // Caret inside the second statement: span covers "SELECT\n  2;"
        // — terminator included, it's part of the statement's place.
        let span = statement_span(text, 13).unwrap();
        assert_eq!(&text[span.clone()], "SELECT\n  2;");
        // Rows derived the way sync_send_mark derives them: lines 3–4.
        let start = text[..span.start].matches('\n').count();
        let end = text[..span.end].matches('\n').count();
        assert_eq!((start, end + 1), (2, 4));
        // In the gap, the bar marks the statement above.
        assert_eq!(&text[statement_span(text, 10).unwrap()], "SELECT 1;");
    }

    #[test]
    fn splits_respect_quotes_and_comments() {
        let text = "SELECT 'a;b'; -- c;\nSELECT 2; /* ; */ SELECT 3";
        let spans = split_statements(text);
        let got: Vec<&str> = spans.iter().map(|s| text[s.clone()].trim()).collect();
        assert_eq!(got, ["SELECT 'a;b';", "-- c;\nSELECT 2;", "/* ; */ SELECT 3"]);
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

    /// Token-soup fuzz for the splitter and the caret's span. A
    /// deterministic xorshift builds thousands of nasty buffers —
    /// unterminated strings, comment edges, $$ bodies, stray `;;`,
    /// unicode — and every one must satisfy the invariants that ARE
    /// the semicolon ruling, rather than any hand-picked example.
    #[test]
    fn fuzz_splitter_invariants() {
        const TOKENS: &[&str] = &[
            "select", "from", "where", "insert", "delete", "t", "x1",
            "1", "22", ",", "(", ")", "*", "=", ";", ";;", " ", "\n",
            "\n\n", "\t", "'a;b'", "'it''s'", "'oops", "\"q;\"", "\"un",
            "--", "-- c;\n", "/*", "*/", "/* ; */", "$$", "$$;$$",
            "$$ ; ", "é;∅", "\u{1F986}", "",
        ];
        let mut seed = 0x00D0C0FFEEu64;
        let mut rng = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..4000 {
            let n = (rng() % 24) as usize;
            let text: String =
                (0..n).map(|_| TOKENS[rng() as usize % TOKENS.len()]).collect();
            let spans = split_statements(&text);
            let mut prev_end = 0;
            for s in &spans {
                // In bounds, ordered, disjoint, on char boundaries.
                assert!(prev_end <= s.start && s.start < s.end && s.end <= text.len());
                let body = text.get(s.clone()).expect("span on char boundary");
                prev_end = s.end;
                // Spans are their trimmed selves, and never terminator-only.
                assert_eq!(body, body.trim());
                assert!(!body.chars().all(|c| c == ';'));
                // Statements begin at top level, so a statement's own
                // text re-splits to exactly itself: no interior
                // top-level `;` can be hiding in a span.
                let again = split_statements(body);
                assert_eq!(again, vec![0..body.len()], "re-split of {body:?}");
            }
            // Everything OUTSIDE the spans is gap: whitespace and
            // dropped terminators only — no statement text ever leaks.
            let mut outside = String::new();
            let mut at = 0;
            for s in &spans {
                outside.push_str(&text[at..s.start]);
                // The terminator belongs to its statement: stray `;`s
                // may trail a CLOSED span (a dropped `;;`), but an
                // open span abandoning its own terminator outside is
                // exactly the bug this hunts.
                if !text[s.clone()].ends_with(';') {
                    assert!(
                        !text[s.end..].trim_start().starts_with(';'),
                        "span {:?} stranded its terminator in {text:?}",
                        &text[s.clone()]
                    );
                }
                at = s.end;
            }
            outside.push_str(&text[at..]);
            assert!(
                outside.chars().all(|c| c.is_whitespace() || c == ';'),
                "leaked {outside:?} from {text:?}"
            );
            // Every caret owns one of the spans (or none when there are
            // none), and the payload is the span minus its terminator.
            for caret in [0, text.len() / 2, text.len(), text.len() + 7] {
                let span = statement_span(&text, caret);
                assert_eq!(span.is_none(), spans.is_empty());
                if let Some(r) = span {
                    assert!(spans.contains(&r));
                    let sql = statement_at(&text, caret).expect("span implies payload");
                    // The payload is the span's own text minus exactly
                    // ONE terminator (a deeper `;` is statement text —
                    // the fuzzer's first scalp was a strip-them-all
                    // implementation), and it is never empty.
                    let body = &text[r];
                    let want = body.strip_suffix(';').map_or(body, str::trim_end);
                    assert!(!sql.is_empty());
                    assert_eq!(sql, want);
                }
            }
        }
    }
}
