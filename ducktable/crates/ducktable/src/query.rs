//! The Query view (docs/QUERY.md): a berth-scoped SQL scratchpad above
//! a read-only results pane. The editor is gpui-component's code editor
//! with tree-sitter-duckdb highlighting; ⌘Enter sends the statement
//! under the caret; the scratch autosaves and survives restarts.
//!
//! One statement per run, one result per run, results paged server-side
//! through the subquery wrap (probe row + on-demand count). A statement
//! that opens a transaction gets a Harbor session, and every run goes
//! through that session until a statement ends the transaction (`Txn`).

use crate::theme::{pal, value_font, CELL_TEXT};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use gpui_kit::component::input::{Editor, EditorState, InputEvent};
use gpui_kit::component::{Sizable as _, StyledExt as _};
use harbor_client::Conn;

pub(crate) struct QueryView {
    conn: Conn,
    berth: String,
    editor: Entity<EditorState>,
    /// The results pane: a REAL Grid, embedded — "a Data window with a
    /// custom query preceding it". It pages, selects, and honors the
    /// display toggles exactly like the Data view, and it is read-only
    /// by construction (no catalog structure, no key, no Edits).
    results: Option<Entity<crate::grid::Grid>>,
    /// A resultless statement's verdict: the engine said ok in N ms.
    ok_ms: Option<u64>,
    /// A transient footer note ("nothing to run"), cleared by the next
    /// verdict.
    note: Option<SharedString>,
    error: Option<SharedString>,
    running: bool,
    /// Which run is in flight: one run's ticking "running" line stops when
    /// the next run has begun, and does not tick on beside that run's own.
    generation: u64,
    /// The transaction this view holds open, as the session its statements
    /// run on. None between transactions, where every run is its own
    /// request and commits on its own.
    txn: Option<Txn>,
    /// The session of a transaction whose opening statement is still in
    /// flight: `txn` is not set until that statement answers, and a quit in
    /// between must still find the session to give it back.
    opening: std::sync::Arc<std::sync::Mutex<Option<Txn>>>,
    /// The transaction was lost while no statement was running: the server
    /// reclaimed its session and rolled it back. The next statement was
    /// typed for that transaction, and sent on its own it would commit on
    /// its own, so the next run is refused, once, with the reason (`route`).
    lost: bool,
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
    split: Entity<gpui_kit::component::resizable::ResizableState>,
    _subscription: Subscription,
    /// Keystroke interceptor for ⌘Enter: it must run BEFORE the input's
    /// own binding, which would insert a newline first (send means send,
    /// not send-and-type).
    _intercept: Subscription,
}

impl EventEmitter<crate::app::CatalogRefreshRequested> for QueryView {}

impl QueryView {
    pub(crate) fn new(
        conn: Conn,
        berth: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut state = EditorState::new(window, cx)
                .language("duckdb")
                .line_number(true)
                .folding(false)
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
        cx.observe_in(&editor, window, |this, editor, window, cx| {
            this.sync_send_mark(editor, window, cx)
        })
        .detach();
        // The editor/results divider persists like every other divider
        // (sidebar, inspector): only the user's drag writes it.
        let split = cx.new(|_| gpui_kit::component::resizable::ResizableState::default());
        cx.subscribe(
            &split,
            |_, state, _: &gpui_kit::component::resizable::ResizablePanelEvent, cx| {
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
            ok_ms: None,
            note: None,
            error: None,
            running: false,
            generation: 0,
            txn: None,
            opening: Default::default(),
            lost: false,
            run_started: None,
            show_running: false,
            needs_focus: false,
            split,
            _subscription: subscription,
            _intercept: intercept,
        };
        // Seed the mark for the restored scratch before first paint.
        this.sync_send_mark(this.editor.clone(), window, cx);
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
    fn sync_send_mark(
        &mut self,
        editor: Entity<EditorState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // value() already materializes the rope into a SharedString;
        // it derefs to &str, so no second copy is taken (this runs at
        // blink frequency — allocations here are pure heat).
        let (text, caret) = {
            let e = editor.read(cx);
            (e.value(), e.cursor())
        };
        // Numbers live only on statement lines, restarting at 1 on
        // each statement — matching the engine's own "LINE n" — and
        // the gap rows between statements carry none (label 0 = silent
        // row). A blank line INSIDE a statement still counts: the
        // engine counts it too.
        let rows = text.matches('\n').count() + 1;
        let mut labels = vec![0u32; rows];
        let stmts = split_statements(&text);
        // The mark's color is the header band's own background: the
        // marked statement's rail cells dim to it — what ⌘Enter will
        // send reads as a PLACE in the margin, not a sticker on it.
        // Picked from the same lex as the labels below — one truth.
        let mark = statement_pick(&stmts, caret.min(text.len())).map(|s| {
            let start = text[..s.span.start].matches('\n').count();
            let end = text[..s.span.end].matches('\n').count();
            let shade = {
                use gpui_kit::component::ActiveTheme as _;
                cx.theme().table_head
            };
            (start..end + 1, shade)
        });
        // Only a `;` closes a band — ANY band, not just the last. The
        // open tail after the final `;` draws no closing hairline: the
        // line would claim "done here" under a mid-air thought. It
        // appears the moment the `;` does. end_rows lists the last row
        // of each statement that earned one. Rows come from a running
        // cursor — statements are ordered and disjoint, so one forward
        // pass counts every newline exactly once (a prefix scan per
        // statement goes quadratic on a pasted dump, and this runs at
        // blink frequency).
        let mut end_rows: Vec<u32> = Vec::new();
        let (mut pos, mut row) = (0usize, 0usize);
        for stmt in &stmts {
            row += text[pos..stmt.span.start].matches('\n').count();
            let r0 = row;
            row += text[stmt.span.clone()].matches('\n').count();
            let r1 = row;
            pos = stmt.span.end;
            for (i, r) in (r0..=r1).enumerate() {
                if labels[r] == 0 {
                    labels[r] = (i + 1) as u32;
                }
            }
            if stmt.terminated {
                end_rows.push(r1 as u32);
            }
        }
        let max_label = labels.iter().copied().max().unwrap_or(1) as u64;
        // The rail obeys ⌥7 exactly like the grids: hidden means GONE
        // (the boundary line above the pane is all that remains).
        let show = crate::prefs::get(cx).row_numbers;
        editor.update(cx, |e, cx| e.set_line_number(show, window, cx));
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
        // The rail's measures count from the editor's outer left edge —
        // the pane's own — so the rail width is used verbatim.
        let t = crate::theme::pal(cx);
        let style = gpui_kit::base::input::GutterStyle {
            width: px(rail),
            right_inset: px(6.),
            text_gap: px(12.),
            text_size: px(crate::theme::GUTTER_TEXT),
            background: t.raised,
            row_line: t.grid_line,
            // The rail's edge, the band boundary, and the statement
            // hairlines are all grid lines — the one slot (red-audit
            // ruling, 2026-09-01).
            border: t.grid_line,
        };
        let stale = {
            let e = editor.read(cx);
            e.marked_rows != mark
                || e.gutter_style.as_ref() != Some(&style)
                || e.section_end_rows.as_deref().map(Vec::as_slice)
                    != Some(end_rows.as_slice())
                || e.line_labels.as_deref().map(Vec::as_slice)
                    != Some(labels.as_slice())
        };
        if stale {
            editor.update(cx, |e, cx| {
                e.marked_rows = mark;
                e.gutter_style = Some(style);
                e.section_end_rows = Some(std::rc::Rc::new(end_rows));
                e.line_labels = Some(std::rc::Rc::new(labels));
                cx.notify();
            });
        }
    }

    fn on_editor_event(
        &mut self,
        _: &Entity<EditorState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::PressEnter { secondary: true, .. } => self.run(window, cx),
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
        // Where this statement runs is decided before it is sent: on the
        // open transaction's session, on a session opened for it because
        // it begins one, as its own request, or nowhere.
        let effect = txn_effect(&sql);
        let route = route(self.txn.is_some(), std::mem::take(&mut self.lost), effect);
        if route == Route::Refused {
            self.error = Some(SharedString::from(LOST_REFUSED));
            self.note = None;
            cx.notify();
            return;
        }
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
        let held = self.txn.clone();
        let opening = self.opening.clone();
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
            let (outcome, total, paged, txn, fate) = cx
                .background_executor()
                .spawn(async move {
                    let txn = match route {
                        Route::Alone | Route::Refused => None,
                        Route::Held => held,
                        Route::Opening => match Txn::open(&conn) {
                            Ok(txn) => {
                                *opening.lock().unwrap_or_else(|p| p.into_inner()) = Some(txn.clone());
                                Some(txn)
                            }
                            // Without a session the statement is not sent:
                            // alone, it would begin a transaction nothing
                            // could join.
                            Err(message) => {
                                let message = format!("no session for the transaction: {message}");
                                return (Err(message), None, false, None, Fate::Closed);
                            }
                        },
                    };
                    let exec = |sql: &str| match &txn {
                        Some(txn) => txn.exec(sql),
                        None => harbor_client::exec_checked(&conn, sql, None, None),
                    };
                    let mut aborted = false;
                    let (outcome, total, paged) = 'run: {
                        // A COMMIT of an aborted transaction answers like any
                        // other and rolls back. So the session is asked
                        // first, and the COMMIT follows in the same turn on
                        // the session, with nothing between the two
                        // (`Txn::commit`).
                        if let (Some(txn), Route::Held, Some(TxnEffect::Commits)) = (&txn, route, effect) {
                            let (was_aborted, answer) = txn.commit(&sql);
                            aborted = was_aborted;
                            break 'run (answer, None, false);
                        }
                        if wrappable(&sql) {
                            let src = crate::sql::query_source(&sql);
                            let probe =
                                exec(&crate::sql::page_sql(&src, false, &None, 0, size + 1));
                            match probe {
                                Ok(mut result) => {
                                    if result.rows.len() <= size {
                                        let total = result.rows.len() as u64;
                                        break 'run (Ok(result), Some(total), true);
                                    }
                                    result.rows.truncate(size);
                                    result.row_count = size as u64;
                                    // A failed count leaves the total unknown;
                                    // the grid's full-page heuristic still
                                    // paces has_next, and the footer reads
                                    // "1–5,000 rows" — honest, not wrong.
                                    // Inside a transaction the failure is the
                                    // verdict instead: the count reads every
                                    // row, an error in a later one aborts the
                                    // transaction, and that must be said.
                                    // A count that never ran changed nothing,
                                    // and leaves the total unknown there too.
                                    let total = match exec(&crate::sql::count_sql(&src, &None)) {
                                        Ok(counted) => crate::sql::count_of(&counted),
                                        Err(failure) if txn.is_some() && !never_ran(&failure) => {
                                            break 'run (Err(failure), None, false);
                                        }
                                        Err(_) => None,
                                    };
                                    break 'run (Ok(result), total, true);
                                }
                                // Inside a transaction the probe's failure is
                                // the verdict, unless the wrap merely failed
                                // to parse: any other error has aborted the
                                // transaction, or its session is gone, and the
                                // bare run would only report that.
                                Err(failure) if txn.is_some() && !never_ran(&failure) => {
                                    break 'run (Err(failure), None, false);
                                }
                                Err(_) => {}
                            }
                        }
                        (exec(&sql), None, false)
                    };
                    let fate = fate(route, effect, aborted, outcome.as_ref().err());
                    // A transaction that is over gives its session back
                    // here, before the verdict shows: releasing rolls back
                    // whatever a failed ending left open.
                    if let (Some(txn), false) = (&txn, fate == Fate::Open) {
                        txn.release();
                    }
                    let outcome = match (fate, outcome) {
                        // The COMMIT answered, and what it did was roll back.
                        (Fate::RolledBack, Ok(_)) => Err(ROLLED_BACK.to_string()),
                        // It did not answer, and rolled back either way.
                        (Fate::RolledBack, Err(failure)) => Err(format!("{failure}\n\n{ROLLED_BACK_UNANSWERED}")),
                        (_, outcome) => outcome.map_err(|failure| failure.to_string()),
                    };
                    (outcome, total, paged, txn, fate)
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                // The session is reachable through `txn` from here on, or
                // was given back above.
                this.opening.lock().unwrap_or_else(|p| p.into_inner()).take();
                let was_open = this.txn.is_some();
                this.txn = if fate == Fate::Open { txn.clone() } else { None };
                if this.txn.is_some() && !was_open {
                    this.watch_transaction(cx);
                }
                // A statement that found the session gone has told the user
                // so itself; the watcher's latch has nothing left to say.
                if fate == Fate::Lost {
                    this.lost = false;
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
                        // BEGIN, COMMIT and their kin answer with an empty
                        // `Success` column; their verdict is the status
                        // line's and the transaction mark's. Under EXPLAIN
                        // ANALYZE they answer with the plan, which shows.
                        if result.columns.is_empty() || effect.is_some() && result.rows.is_empty() {
                            this.results = None;
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
                            // Later pages of a result read inside the
                            // transaction are read inside it too, while
                            // it lasts.
                            let session = this.txn.clone();
                            let grid = cx.new(|cx| {
                                let mut grid = crate::grid::Grid::new_query(
                                    conn,
                                    &sql_logged,
                                    Ok(result),
                                    grid_total,
                                    page_size,
                                    paged,
                                    window,
                                    cx,
                                );
                                grid.session = session;
                                grid
                            });
                            this.results = Some(grid);
                        }
                    }
                    Err(message) => {
                        let message = match fate.note(effect) {
                            Some(note) => format!("{message}\n\n{note}"),
                            None => message,
                        };
                        this.error = Some(SharedString::from(message));
                        this.results = None;
                        this.ok_ms = None;
                    }
                }
                // Arbitrary SQL may change any table. Request a catalog
                // refresh after every completed send, including an error:
                // an earlier statement may have committed before a later
                // statement failed.
                cx.emit(crate::app::CatalogRefreshRequested);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Keep the open transaction's session from idling out, and notice when
    /// it is gone. Harbor reclaims a session that sits thirty seconds between
    /// statements, which a person composing the next one easily does, so a
    /// trivial statement goes to it at a third of that interval while nothing
    /// else is running on it. The session's fixed deadline cannot be
    /// extended: when it passes, the server has rolled the transaction back,
    /// and the view says so. One watcher per transaction; it ends with it.
    fn watch_transaction(&mut self, cx: &mut Context<Self>) {
        let Some(txn) = self.txn.clone() else { return };
        cx.spawn(async move |this, cx| {
            let tick = std::time::Duration::from_secs(1);
            let mut quiet = std::time::Duration::ZERO;
            loop {
                cx.background_executor().timer(tick).await;
                let running = this.update(cx, |this, cx| {
                    let mine = this.txn.as_ref().is_some_and(|open| open.is(&txn));
                    // The mark counts down, so it repaints each second.
                    cx.notify();
                    mine.then_some(this.running)
                });
                let Ok(Some(running)) = running else { break };
                // A statement in flight is activity, and its verdict will
                // say what became of the session.
                if running {
                    quiet = std::time::Duration::ZERO;
                    continue;
                }
                // Gone past its deadline, or found gone by a results grid's
                // page since the last tick.
                let mut gone = txn.over() || txn.remaining().is_zero();
                quiet += tick;
                if !gone && !txn.idle().is_zero() && quiet >= txn.idle() / 3 {
                    quiet = std::time::Duration::ZERO;
                    let session = txn.clone();
                    gone = cx.background_executor().spawn(async move { session.touch() }).await
                        == Touch::Gone;
                }
                if gone {
                    // Past its deadline or unknown to the server, the session
                    // takes no more statements: marking it over sends the
                    // pages of a results grid that holds it the ordinary way,
                    // and the release makes sure nothing is left open on it.
                    let session = txn.clone();
                    cx.background_executor().spawn(async move { session.release() }).await;
                    this.update(cx, |this, cx| {
                        if this.txn.as_ref().is_some_and(|open| open.is(&txn)) {
                            this.txn = None;
                            this.lost = true;
                            this.error = Some(SharedString::from(LOST));
                            cx.notify();
                        }
                    })
                    .ok();
                    break;
                }
            }
        })
        .detach();
    }

    /// Whether a transaction is open here: what a quit would roll back.
    pub(crate) fn in_transaction(&self) -> bool {
        self.txn.is_some()
    }

    /// Whether a statement is in flight: what a quit would stop watching.
    pub(crate) fn is_running(&self) -> bool {
        self.running
    }

    /// The user chose to quit: what gives this view's session back, so the
    /// server rolls its transaction back at once instead of at its idle
    /// timeout. The session is the open transaction's, or that of one whose
    /// opening statement is still in flight; releasing it cancels a
    /// statement running there. Each closure blocks on one request, and the
    /// caller runs it off the main thread.
    pub(crate) fn release_for_quit(&mut self) -> Vec<Box<dyn FnOnce() + Send>> {
        let opening = self.opening.lock().unwrap_or_else(|p| p.into_inner()).take();
        [self.txn.take(), opening]
            .into_iter()
            .flatten()
            .map(|txn| Box::new(move || txn.release()) as Box<dyn FnOnce() + Send>)
            .collect()
    }

    /// The transaction mark: that one is open, and how long the server
    /// will keep it. None between transactions.
    fn transaction_mark(&self) -> Option<(String, Health)> {
        let txn = self.txn.as_ref()?;
        Some((transaction_mark(txn.health(), txn.remaining()), txn.health()))
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
            self.editor.read(cx).focus_handle(cx).focus(window, cx);
        }
        // Results arriving can widen the shared rail; the guarded write
        // makes this free on every other frame.
        self.sync_send_mark(self.editor.clone(), window, cx);
        div()
            .size_full()
            .min_h_0()
            .v_flex()
            .child({
                // The grids' header row, echoed: same height, same
                // paint, a "#" on the number rail — so ⌘1/2/3 keeps
                // the chrome still and only the content changes.
                let row_h = crate::prefs::get(cx).table_size().table_row_height();
                // The rail's width, so "#" lands on the numbers' right
                // edge exactly like the grids'.
                let gw = self
                    .editor
                    .read(cx)
                    .gutter_style
                    .as_ref()
                    .map(|g| g.width)
                    .unwrap_or(px(crate::grid::gutter_width(1)));
                div()
                    .flex_none()
                    .w_full()
                    .h(row_h)
                    .h_flex()
                    .items_center()
                    // The band OWNS its bottom border, exactly like the
                    // grids' header row — border inside row_h, so the
                    // "#" centers in row_h minus the border px and the
                    // line is full height by construction. The editor
                    // paints no boundary of its own at its top: a paint
                    // there clips to its half-pixel content bounds and
                    // renders half height, and row 0 opens with no
                    // hairline, so the boundary is one line, never two.
                    .border_b_1()
                    .border_color(t.grid_line)
                    .bg({
                        use gpui_kit::component::ActiveTheme as _;
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
                            .border_color(t.grid_line)
                            .child(
                                div()
                                    .w_full()
                                    .text_right()
                                    // 5, not 6: the grids' rail divider
                                    // is a painted overlay taking no
                                    // layout space, so their "#" ends
                                    // 5px before the line; border_r_1
                                    // here DOES take a px, and 6 would
                                    // land the text 1px further left.
                                    .pr(px(5.))
                                    .text_size(px(crate::theme::GUTTER_TEXT))
                                    .font_family(value_font())
                                    .text_color(t.muted)
                                    .child("#"),
                            ),
                    ))
                    // A transaction held open is state the user must not
                    // lose sight of: it sits in the band, above the text
                    // that will run inside it, for as long as it lasts.
                    .when_some(self.transaction_mark(), |d, (mark, health)| {
                        d.child(
                            div()
                                .flex_1()
                                .px_3()
                                .text_right()
                                .text_xs()
                                .text_color(if health == Health::Aborted { t.bad } else { t.warn })
                                .child(mark),
                        )
                    })
            })
            .map(|d| {
                // The editor pane: the scratchpad, in the value font.
                // Flush at the pane's left so the imposed gutter
                // (sync_send_mark) sits exactly on the grids' number
                // rail; the editor takes its smallest insets (.xsmall()
                // below), none above or below. Line height is the grids'
                // row height, so line 1 sits where row 1 sits.
                let editor_pane = div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    // Half a logical pixel up: the grids center row
                    // content in row_h minus the 1px bottom border,
                    // the editor in the full line box — this is the
                    // difference, measured on screen. The editor's
                    // hairlines do NOT ride the half pixel with the
                    // text: the gutter paints them on whole pixels. No
                    // bottom padding: the rail runs all the way to the
                    // footer.
                    .mt(px(-0.5))
                    .font_family(value_font())
                    // The same zoom ladder as the Data grid (Cmd-= / -),
                    // set ON the input: Input applies its own size-class
                    // text_sm before refining with caller styles, so a
                    // wrapper cascade never reaches the editor text.
                    .child(
                        Editor::new(&self.editor)
                            .h_full()
                            // The smallest insets: none above or below,
                            // so the rail runs from the header band to
                            // the footer with no dead strips. The rail
                            // paints from the editor's outer edge and
                            // its hairlines reach the outer right edge;
                            // the gutter supplies the text's inset.
                            .xsmall()
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
                        gpui_kit::base::v_resizable("query-split")
                            .with_state(&self.split)
                            .child(
                                gpui_kit::component::resizable::resizable_panel()
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
                                gpui_kit::component::resizable::resizable_panel().child(
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
/// SELECT-shaped family, and for the statements that answer with a table
/// of their own (DESCRIBE, SUMMARIZE, SHOW, PIVOT, UNPIVOT). Anything else
/// stays a single inert page. A wrong yes costs one failed probe, after
/// which the statement runs bare.
fn wrappable(sql: &str) -> bool {
    // Read as the engine reads it (`wire::statement`): past the spaces and
    // comments it skips, to the first bare word.
    let (bytes, mut at) = (sql.as_bytes(), 0);
    wire::statement::skip_trivia(bytes, &mut at);
    if bytes.get(at) == Some(&b'(') {
        return true;
    }
    matches!(
        wire::statement::bare_word(bytes, &mut at).as_str(),
        "SELECT" | "WITH" | "FROM" | "VALUES" | "TABLE" | "DESCRIBE" | "DESC" | "SUMMARIZE"
            | "SHOW" | "PIVOT" | "UNPIVOT"
    )
}

/// The band's mark while a transaction is open: its state, how long the
/// server will keep it, and what ends it.
fn transaction_mark(health: Health, left: std::time::Duration) -> String {
    let left = left.as_secs();
    let (state, ends) = match health {
        Health::Fine => ("transaction open", "COMMIT or ROLLBACK ends it"),
        Health::Aborted => ("transaction aborted by an error", "ROLLBACK ends it"),
        Health::Unknown => {
            ("transaction open, its state unconfirmed after an error", "COMMIT asks first, ROLLBACK ends it")
        }
    };
    format!("{state} \u{00b7} {}:{:02} left \u{00b7} {ends}", left / 60, left % 60)
}

// ============================ transactions ============================

/// A transaction the Query view holds open, as the Harbor session that
/// pins its connection (docs/QUERY.md, "Transactions"). One request carries
/// one statement on a pooled connection, so a `BEGIN` sent alone would
/// open a transaction nothing could ever join; the view opens a session
/// for it instead and sends every later statement there, until one ends
/// the transaction and the session goes back.
///
/// Clones share the session. The last one dropped gives it back if no
/// statement ended it first — the view closed, the connection dropped,
/// another database was chosen — and releasing rolls back.
#[derive(Clone)]
pub(crate) struct Txn(std::sync::Arc<Held>);

struct Held {
    conn: Conn,
    session: harbor_client::Session,
    opened: std::time::Instant,
    /// One statement at a time: Harbor refuses a second on a busy session,
    /// and the keepalive must not be the one that makes a run fail.
    gate: std::sync::Mutex<()>,
    /// Given back, or found gone. Nothing more runs on it.
    over: std::sync::atomic::AtomicBool,
    /// Whether an error has aborted the transaction (`Health`), as the
    /// session last said when asked. Asked after any statement on it that
    /// may have run and failed, by every keepalive, and before a COMMIT.
    health: std::sync::atomic::AtomicU8,
}

/// What is known of the transaction on a session. An aborted transaction
/// stays open until ROLLBACK or COMMIT ends it, answers `SELECT 1` with
/// "Current transaction is aborted", and a COMMIT of it rolls back. Only that
/// question settles it: other statements can answer on an aborted
/// transaction (PREPARE does), so a success proves nothing.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Health {
    /// The session answered `SELECT 1`.
    Fine,
    /// It answered that the transaction is aborted.
    Aborted,
    /// A statement may have run and failed, and the session could not be
    /// asked since.
    Unknown,
}

/// What asking the session found.
#[derive(Debug, PartialEq)]
enum Asked {
    Is(Health),
    /// Harbor no longer knows the session.
    Gone(harbor_client::Failure),
    /// No verdict: the session was busy, or the question got no answer.
    /// The reason.
    Unanswered(String),
}

/// Read the session's answer to `SELECT 1`.
fn asked(answer: Result<harbor_client::QueryResult, harbor_client::Failure>) -> Asked {
    use harbor_client::Failure;
    match answer {
        Ok(_) => Asked::Is(Health::Fine),
        Err(failure) if failure.session_gone() => Asked::Gone(failure),
        Err(Failure::Refused { code, message }) if code == "sql_error" && message.contains(ABORTED) => {
            Asked::Is(Health::Aborted)
        }
        Err(failure) => Asked::Unanswered(failure.to_string()),
    }
}

/// The engine's words for every statement on an aborted transaction.
const ABORTED: &str = "transaction is aborted";

/// Whether a COMMIT may be sent, given what the session said just before:
/// `Ok(aborted)` to send it, knowing what it will do, or the failure to
/// report in its place. A COMMIT whose transaction's state could not be
/// confirmed is not sent: it might roll everything back and answer like one
/// that committed, and a verdict resting on an earlier guess would be one.
fn commit_gate(asked: Asked) -> Result<bool, harbor_client::Failure> {
    match asked {
        Asked::Is(health) => Ok(health == Health::Aborted),
        Asked::Gone(failure) => Err(failure),
        Asked::Unanswered(why) => Err(harbor_client::Failure::Unsent(format!(
            "could not confirm the transaction's state ({why}), so the COMMIT was not sent: \
             try again"
        ))),
    }
}

/// What a keepalive found.
#[derive(Debug, PartialEq)]
enum Touch {
    /// The session answered, or was busy with a statement, or the request
    /// did not get through: nothing says it is gone.
    Alive,
    /// Harbor no longer knows the session.
    Gone,
}

impl Txn {
    fn open(conn: &Conn) -> Result<Self, String> {
        let session = harbor_client::session_open(conn)?;
        Ok(Self(std::sync::Arc::new(Held {
            conn: conn.clone(),
            session,
            opened: std::time::Instant::now(),
            gate: std::sync::Mutex::new(()),
            over: std::sync::atomic::AtomicBool::new(false),
            health: std::sync::atomic::AtomicU8::new(Health::Fine as u8),
        })))
    }

    fn is(&self, other: &Txn) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }

    fn over(&self) -> bool {
        self.0.over.load(std::sync::atomic::Ordering::Acquire)
    }

    /// How long until the session's fixed deadline, when Harbor rolls the
    /// transaction back whatever is running.
    fn remaining(&self) -> std::time::Duration {
        self.0.session.ttl.saturating_sub(self.0.opened.elapsed())
    }

    fn idle(&self) -> std::time::Duration {
        self.0.session.idle
    }

    fn health(&self) -> Health {
        match self.0.health.load(std::sync::atomic::Ordering::Acquire) {
            0 => Health::Fine,
            1 => Health::Aborted,
            _ => Health::Unknown,
        }
    }

    fn set_health(&self, health: Health) {
        self.0.health.store(health as u8, std::sync::atomic::Ordering::Release);
    }

    /// Run one statement on the session. Blocks while another is running.
    fn exec(&self, sql: &str) -> Result<harbor_client::QueryResult, harbor_client::Failure> {
        let _turn = self.0.gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        self.exec_in_turn(sql)
    }

    /// `exec`, with the gate held by the caller. A statement that may have
    /// run and failed may have aborted the transaction, so the session is
    /// asked at once, in the same turn: the band is then never a guess, and
    /// never says aborted of a statement Harbor itself turned away.
    fn exec_in_turn(&self, sql: &str) -> Result<harbor_client::QueryResult, harbor_client::Failure> {
        let answer = harbor_client::exec_checked(&self.0.conn, sql, None, Some(&self.0.session.id));
        match answer.as_ref().err().map(ran) {
            _ if answer.as_ref().is_err_and(|failure| failure.session_gone()) => self.mark_over(),
            None | Some(Ran::Never) => {}
            // No answer: the statement may still be running there, and the
            // session would only answer that it is busy.
            Some(Ran::Unknown) if matches!(answer, Err(harbor_client::Failure::Unanswered(_))) => {
                self.set_health(Health::Unknown)
            }
            Some(Ran::Failed | Ran::Unknown) => {
                if let Asked::Unanswered(_) = self.ask() {
                    self.set_health(Health::Unknown);
                }
            }
        }
        answer
    }

    /// Run a statement for a results grid that pages inside the
    /// transaction. Once the transaction is over, its pages are read like
    /// any others.
    pub(crate) fn page(&self, sql: &str) -> Result<harbor_client::QueryResult, String> {
        if self.over() {
            return harbor_client::query(&self.0.conn, sql);
        }
        match self.exec(sql) {
            // The server reclaimed the session since the last statement: the
            // transaction is over, and this page is read like any other.
            Err(failure) if failure.session_gone() => {
                self.mark_over();
                harbor_client::query(&self.0.conn, sql)
            }
            other => other.map_err(|failure| failure.to_string()),
        }
    }

    /// Nothing more runs on the session: it is gone, without a release of
    /// ours having ended it.
    fn mark_over(&self) {
        self.0.over.store(true, std::sync::atomic::Ordering::Release);
    }

    /// Reset the session's idle clock with a statement that changes
    /// nothing, unless one is already running on it. It answers at once or
    /// not at all, so it waits a few seconds and no longer: the gate it
    /// holds meanwhile is the one the next run waits on.
    fn touch(&self) -> Touch {
        if self.over() {
            return Touch::Gone;
        }
        let Ok(_turn) = self.0.gate.try_lock() else { return Touch::Alive };
        self.ask();
        if self.over() { Touch::Gone } else { Touch::Alive }
    }

    /// Send a COMMIT, knowing what it will do: ask the session whether its
    /// transaction is aborted, and send the COMMIT in the same turn, so no
    /// other statement on the session (a results grid's page, a keepalive)
    /// comes between the question and the COMMIT and aborts what was just
    /// found sound. Returns whether the transaction was aborted, and the
    /// COMMIT's answer; when the session's state cannot be confirmed the
    /// COMMIT is not sent (`commit_gate`).
    fn commit(&self, sql: &str) -> (bool, Result<harbor_client::QueryResult, harbor_client::Failure>) {
        let _turn = self.0.gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        match commit_gate(self.ask()) {
            Ok(aborted) => (aborted, self.exec_in_turn(sql)),
            Err(failure) => (false, Err(failure)),
        }
    }

    /// `SELECT 1` on the session, with the gate held by the caller, and
    /// what it says of the transaction kept. A question that gets no
    /// verdict changes nothing that was known.
    fn ask(&self) -> Asked {
        let found = asked(harbor_client::exec_within(
            &self.0.conn,
            "SELECT 1",
            None,
            Some(&self.0.session.id),
            std::time::Duration::from_secs(5),
        ));
        match &found {
            Asked::Is(health) => self.set_health(*health),
            Asked::Gone(_) => self.mark_over(),
            Asked::Unanswered(_) => {}
        }
        found
    }

    /// Give the session back, once. Harbor rolls back whatever is open on
    /// it, and cancels a statement still running there.
    fn release(&self) {
        self.0.release();
    }
}

impl Held {
    fn release(&self) {
        if !self.over.swap(true, std::sync::atomic::Ordering::AcqRel) {
            harbor_client::session_release(&self.conn, &self.session.id);
        }
    }
}

impl Drop for Held {
    /// The last holder is gone without a statement having ended the
    /// transaction. The release is a blocking request, so it runs off the
    /// thread that dropped the view.
    fn drop(&mut self) {
        if !self.over.swap(true, std::sync::atomic::Ordering::AcqRel) {
            let (conn, id) = (self.conn.clone(), self.session.id.clone());
            std::thread::spawn(move || harbor_client::session_release(&conn, &id));
        }
    }
}

/// What a statement does to the transaction around it, read from the
/// keyword the engine acts on (`wire::statement`, shared with the server
/// and Harbor's own client, so the three never disagree on a statement).
#[derive(Clone, Copy, Debug, PartialEq)]
enum TxnEffect {
    /// `BEGIN`, `START TRANSACTION`.
    Opens,
    /// `COMMIT`, `END`.
    Commits,
    /// `ROLLBACK`, `ABORT`.
    RollsBack,
}

impl TxnEffect {
    fn ends(self) -> bool {
        self != TxnEffect::Opens
    }
}

fn txn_effect(sql: &str) -> Option<TxnEffect> {
    match wire::statement::acting_keyword(sql).as_str() {
        "BEGIN" | "START" => Some(TxnEffect::Opens),
        "COMMIT" | "END" => Some(TxnEffect::Commits),
        "ROLLBACK" | "ABORT" => Some(TxnEffect::RollsBack),
        _ => None,
    }
}

/// Where a statement runs.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Route {
    /// As its own request, committed on its own.
    Alone,
    /// On the session of the transaction already open, whatever it is: a
    /// second `BEGIN` there is the engine's to refuse.
    Held,
    /// On a session opened for it, because it begins a transaction.
    Opening,
    /// Nowhere: the transaction it was typed for was lost since the last
    /// run, and the user has not been told at a run yet.
    Refused,
}

/// Where a statement runs. `lost` is the latch the watcher raises when it
/// finds the transaction gone between statements; the run that reads it
/// lowers it, so the refusal is said once and the run after it goes ahead.
fn route(open: bool, lost: bool, effect: Option<TxnEffect>) -> Route {
    match (open, lost, effect) {
        (true, ..) => Route::Held,
        (false, true, _) => Route::Refused,
        (false, false, Some(TxnEffect::Opens)) => Route::Opening,
        (false, false, _) => Route::Alone,
    }
}

/// What has become of the transaction once a statement has its verdict.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Fate {
    /// One is open: still, or as of this statement.
    Open,
    /// None is open, and nothing needs saying: there was none, a statement
    /// ended it, or the statement that would have begun it failed.
    Closed,
    /// A COMMIT answered, and the transaction it ended had been aborted by
    /// an earlier error: it rolled back, and nothing of it was kept.
    RolledBack,
    /// An ending statement failed with the engine's own error (`sql_error`).
    /// Measured: a COMMIT the engine refuses ends the transaction rolled back.
    Failed,
    /// An ending statement found no transaction on the session: the engine
    /// had ended it already, on a statement the view did not read as one
    /// that ends a transaction. Nothing was rolled back by this one.
    NoneActive,
    /// A COMMIT Harbor answered `cancelled`: it never started, Harbor
    /// aborted the transaction, and nothing since BEGIN was kept.
    NotKept,
    /// An ending statement got no answer, so the transaction may have
    /// ended either way.
    InDoubt,
    /// A statement sent on its own got no answer: it may have run, and on
    /// its own it commits.
    MaybeRan,
    /// Harbor no longer knows the session: it reclaimed it, at its idle
    /// timeout or its deadline, and rolled the transaction back. The
    /// statement did not run.
    Lost,
}

/// `aborted` is what the session answered when asked just before a COMMIT
/// (`Txn::commit`); it matters to no other statement.
fn fate(
    route: Route,
    effect: Option<TxnEffect>,
    aborted: bool,
    failure: Option<&harbor_client::Failure>,
) -> Fate {
    use harbor_client::Failure;
    let ends = effect.is_some_and(TxnEffect::ends);
    let doomed = aborted && effect == Some(TxnEffect::Commits);
    match (route, failure) {
        // A statement sent on its own that got no answer may have run, and
        // on its own it commits.
        (Route::Alone, Some(failure)) if effect.is_none() && ran(failure) == Ran::Unknown => Fate::MaybeRan,
        (Route::Alone | Route::Refused, _) => Fate::Closed,
        (Route::Opening, None) => Fate::Open,
        (Route::Opening, Some(_)) => Fate::Closed,
        (Route::Held, Some(failure)) if failure.session_gone() => Fate::Lost,
        // Any statement but one that ends it leaves it open. An error may
        // have aborted it; the band says so, and ROLLBACK or COMMIT still
        // ends it.
        (Route::Held, _) if !ends => Fate::Open,
        (Route::Held, None) if doomed => Fate::RolledBack,
        (Route::Held, None) => Fate::Closed,
        (Route::Held, Some(failure)) => match ran(failure) {
            // A statement that never ran never reached the transaction: one
            // the parser refused, one Harbor refused before the engine saw
            // it, as it does while the session is still busy with the
            // statement before (`session_busy`) or while it is not serving
            // (`unavailable`), and one that could not be sent. The message
            // is the verdict, and the transaction is as it was.
            Ran::Never => Fate::Open,
            Ran::Failed => match failure {
                Failure::Refused { message, .. } if message.contains(NONE_ACTIVE) => Fate::NoneActive,
                _ => Fate::Failed,
            },
            // No verdict on a COMMIT of an aborted transaction leaves no
            // doubt: it rolls back when it runs, and when its session is
            // released if it did not.
            Ran::Unknown if doomed => Fate::RolledBack,
            // A COMMIT runs to its answer, so one Harbor answered `cancelled`
            // never started: nothing since BEGIN was kept
            // (`edits::commit_outcome`, the grid's rule too).
            Ran::Unknown
                if effect == Some(TxnEffect::Commits)
                    && crate::edits::commit_outcome(Some(failure)) == crate::edits::CommitOutcome::NotLanded =>
            {
                Fate::NotKept
            }
            Ran::Unknown => Fate::InDoubt,
        },
    }
}

/// The engine's words when COMMIT or ROLLBACK finds no transaction.
const NONE_ACTIVE: &str = "no transaction is active";

impl Fate {
    /// What the view adds under a failed statement's error, when the error
    /// alone would leave the transaction's state unsaid.
    fn note(self, effect: Option<TxnEffect>) -> Option<&'static str> {
        match self {
            // RolledBack carries its own words (`ROLLED_BACK`).
            Fate::Open | Fate::Closed | Fate::RolledBack => None,
            Fate::Failed => Some(
                "The transaction is over: its changes were rolled back.",
            ),
            Fate::NoneActive => Some(
                "The session held no transaction: an earlier statement had ended it, and this \
                 one changed nothing. The session was released.",
            ),
            // A rollback rolls back either way: by the statement, or by the
            // release of its session.
            Fate::NotKept => Some(
                "The COMMIT did not run, and nothing since BEGIN was kept. The session was \
                 released.",
            ),
            Fate::MaybeRan => Some(
                "No answer came back: the statement may have run, and on its own it commits. \
                 Look before running it again.",
            ),
            Fate::InDoubt if effect == Some(TxnEffect::RollsBack) => Some(
                "No answer came back. The session was released, which rolls the transaction \
                 back if the statement had not already.",
            ),
            Fate::InDoubt => Some(
                "No answer came back, so the transaction may have ended either way. Its session \
                 was released, which rolls back anything still open.",
            ),
            Fate::Lost if effect.is_some_and(TxnEffect::ends) => Some(LOST),
            Fate::Lost => Some(
                "The transaction is gone: the server reclaimed its session and rolled back \
                 everything since BEGIN. This statement did not run.",
            ),
        }
    }
}

/// A COMMIT that answered on a transaction an earlier error had aborted.
const ROLLED_BACK: &str = "COMMIT rolled back: an earlier error aborted the transaction, and \
                           nothing since BEGIN was kept.";

/// A COMMIT that got no answer on a transaction an earlier error had aborted.
const ROLLED_BACK_UNANSWERED: &str = "An earlier error had aborted the transaction, so it is rolled \
                                      back either way: by the COMMIT if it ran, and by the release \
                                      of its session if not. Nothing since BEGIN was kept.";

/// The transaction's session is gone, found by the keepalive or by a
/// statement that would have ended it.
const LOST: &str = "The transaction is gone: the server reclaimed its session and rolled back \
                    everything since BEGIN.";

/// The run after the keepalive found the transaction gone.
const LOST_REFUSED: &str = "The transaction is gone: the server reclaimed its session and rolled \
                            back everything since BEGIN. This statement was not sent: outside a \
                            transaction it commits on its own. \u{2318}Enter again runs it that way.";

/// Whether a failed statement ran.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Ran {
    /// It did not: it could not be sent, Harbor refused it before the engine
    /// saw it, or the engine's parser did. Measured: a Parser Error inside a
    /// transaction leaves it as it was.
    Never,
    /// It ran, and the engine refused it (`sql_error`). Measured: inside a
    /// transaction every class of engine error but the parser's aborts it:
    /// Catalog, Binder, Constraint, Conversion, Out of Range, Invalid Input
    /// and TransactionContext.
    Failed,
    /// It may have run. No answer came, or Harbor's answer is one it sends
    /// about a statement the engine had begun: `cancelled`, when a deadline
    /// or a cancel interrupts it, `internal`, and a result it could not
    /// send. A code this client does not know is read the same way: nothing
    /// is assumed not to have run.
    Unknown,
}

fn ran(failure: &harbor_client::Failure) -> Ran {
    use harbor_client::Failure;
    match failure {
        Failure::Unsent(_) => Ran::Never,
        Failure::Unanswered(_) => Ran::Unknown,
        Failure::Refused { code, message } => match code.as_str() {
            "sql_error" if message.starts_with("Parser Error") => Ran::Never,
            "sql_error" => Ran::Failed,
            // What Harbor answers before the statement reaches the engine.
            "bad_request" | "not_found" | "forbidden" | "body_too_large" | "no_such_session"
            | "session_busy" | "query_id_in_use" | "no_lease_connections" | "no_lease_available"
            | "unavailable" | "unready" => Ran::Never,
            _ => Ran::Unknown,
        },
    }
}

/// The statement did not run (`Ran::Never`).
fn never_ran(failure: &harbor_client::Failure) -> bool {
    ran(failure) == Ran::Never
}

// =========================== statement spans ==========================

/// One statement, as the splitter sees it. `span` is the statement's
/// PLACE in the buffer — trimmed, INCLUDING its terminating `;` and
/// any same-line trailing `--` comment (docs/QUERY.md: the annotation
/// rides the statement it annotates, so a caret inside it marks the
/// statement above, never the one below). `payload` is what the wire
/// gets: everything before the `;`, trimmed — the pager's subquery
/// wrap (`SELECT * FROM (…) LIMIT n`) can't syntactically hold a
/// terminator or a trailing comment. `terminated` says whether a `;`
/// actually closed it — the band's closing hairline answers to this.
#[derive(Clone, PartialEq, Debug)]
struct Stmt {
    span: std::ops::Range<usize>,
    payload: std::ops::Range<usize>,
    terminated: bool,
}

/// The statement owning `caret`: the last one that begins at or before
/// it — so the gap after a statement still belongs to it, and before
/// the first statement the caret looks down (docs/QUERY.md).
fn statement_pick(stmts: &[Stmt], caret: usize) -> Option<&Stmt> {
    let pick = stmts.iter().rposition(|s| s.span.start <= caret).unwrap_or(0);
    stmts.get(pick)
}

/// The caret's statement as a byte range — what the send mark spans.
/// The app derives it through statement_pick inside sync_send_mark;
/// this standalone reader is the tests' window into the same rule.
#[cfg(test)]
fn statement_span(text: &str, caret: usize) -> Option<std::ops::Range<usize>> {
    let stmts = split_statements(text);
    statement_pick(&stmts, caret.min(text.len())).map(|s| s.span.clone())
}

/// The caret's payload — what ⌘Enter sends. Both readers go through
/// statement_pick, so the bar can never lie about the payload.
fn statement_at(text: &str, caret: usize) -> Option<String> {
    let stmts = split_statements(text);
    statement_pick(&stmts, caret.min(text.len()))
        .filter(|s| !s.payload.is_empty())
        .map(|s| text[s.payload.clone()].to_string())
}

/// The editor and its embedded results grid are one vertical pane, so
/// their row-number rails have one width derived from current content.
fn shared_gutter_max(top: u64, bottom: u64) -> u64 {
    top.max(bottom)
}

/// The statements of the buffer, in order. A tiny lexer, not a parser:
/// it only needs to know what a boundary does NOT end — strings
/// (''-doubled, and e'…' backslash-escaped), quoted identifiers, both
/// comment forms (block comments nest, as in the engine), and
/// dollar-quoted bodies (`$$…$$` and tagged `$tag$…$tag$`).
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
fn split_statements(text: &str) -> Vec<Stmt> {
    let bytes = text.as_bytes();
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut raw: Vec<(std::ops::Range<usize>, usize, bool)> = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' => {
                let quote = bytes[i];
                // e'…' escapes with backslashes; a plain '…' does not.
                let estring = quote == b'\''
                    && i > 0
                    && matches!(bytes[i - 1], b'e' | b'E')
                    && (i < 2 || !ident(bytes[i - 2]));
                i += 1;
                while i < bytes.len() {
                    if estring && bytes[i] == b'\\' {
                        i += 2;
                        continue;
                    }
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
                // Block comments NEST (Postgres heritage): the first
                // `*/` may close an inner comment, not this one.
                let mut depth = 1usize;
                i += 2;
                while i + 1 < bytes.len() && depth > 0 {
                    if bytes[i] == b'/' && bytes[i + 1] == b'*' {
                        depth += 1;
                        i += 2;
                    } else if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                if depth > 0 {
                    // Unterminated: the comment owns the rest of the
                    // buffer — its final byte included (the loop's
                    // two-byte window never examines it).
                    i = bytes.len();
                }
                continue;
            }
            b'$' => {
                // A dollar-quote delimiter is `$tag$` where tag is a
                // (possibly empty) identifier not starting with a
                // digit — `$1` is a parameter, not a quote. The body
                // runs to the EXACT same delimiter.
                let mut j = i + 1;
                while j < bytes.len() && ident(bytes[j]) {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'$' && !bytes[i + 1].is_ascii_digit() {
                    let delim = &bytes[i..=j];
                    let body = j + 1;
                    i = match bytes[body..]
                        .windows(delim.len())
                        .position(|w| w == delim)
                    {
                        Some(k) => body + k + delim.len(),
                        None => bytes.len(),
                    };
                    continue;
                }
            }
            b';' => {
                // The terminator BELONGS to its statement (Steve's
                // ruling): a `;` on its own line is the statement's
                // last row, not a stray gap row outside the band. So
                // does a same-line trailing `-- comment` — the
                // annotation rides the statement it annotates.
                let payload_end = i;
                let mut j = i + 1;
                while j < bytes.len() && matches!(bytes[j], b' ' | b'\t') {
                    j += 1;
                }
                if bytes.get(j) == Some(&b'-') && bytes.get(j + 1) == Some(&b'-') {
                    while j < bytes.len() && bytes[j] != b'\n' {
                        j += 1;
                    }
                } else {
                    j = i + 1;
                }
                raw.push((start..j, payload_end, true));
                start = j;
                i = j;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    if start < bytes.len() {
        raw.push((start..bytes.len(), bytes.len(), false));
    }
    // Shrink each span to its trimmed content: the whitespace between
    // statements must not belong to the NEXT one (the gap rule above).
    // A span that is nothing but terminators (a stray `;;`) is no
    // statement at all. The payload trims independently — it can be
    // empty (a `;` whose only company is its trailing comment).
    raw.into_iter()
        .filter_map(|(s, payload_end, terminated)| {
            let t = &text[s.clone()];
            let a = s.start + (t.len() - t.trim_start().len());
            let b = s.start + t.trim_end().len();
            if a >= b || text[a..b].chars().all(|c| c == ';') {
                return None;
            }
            let p_end = payload_end.clamp(a, b);
            let p_end = a + text[a..p_end].trim_end().len();
            Some(Stmt { span: a..b, payload: a..p_end, terminated })
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
    use super::{
        asked, commit_gate, fate, never_ran, ran, route, shared_gutter_max, split_statements,
        statement_at, statement_span, transaction_mark, txn_effect, wrappable, Asked, Fate, Health,
        Ran, Route, TxnEffect,
    };
    use harbor_client::Failure;

    fn split(t: &str) -> Vec<String> {
        split_statements(t).iter().map(|s| t[s.span.clone()].to_string()).collect()
    }

    #[test]
    fn semicolons_are_the_only_divider() {
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
    fn trailing_comments_ride_the_statement_above() {
        // The annotate-then-run flow: a same-line trailing comment is
        // part of the statement it annotates, so the caret at the end
        // still sends the statement — never a bare comment (a
        // guaranteed engine error), never the statement BELOW.
        let text = "select 1; -- note";
        assert_eq!(split(text), [text]);
        assert_eq!(statement_at(text, text.len()).as_deref(), Some("select 1"));
        let two = "select 1; -- note\nselect 2;";
        assert_eq!(split(two), ["select 1; -- note", "select 2;"]);
        // Caret inside the trailing comment: the statement ABOVE owns
        // it (docs/QUERY.md), and the mark spans comment and all.
        assert_eq!(statement_at(two, 13).as_deref(), Some("select 1"));
        assert_eq!(&two[statement_span(two, 13).unwrap()], "select 1; -- note");
        // A comment on its OWN line is not trailing — it opens the
        // next scribble, exactly as before.
        assert_eq!(
            split("select 1;\n-- next\nselect 2;"),
            ["select 1;", "-- next\nselect 2;"]
        );
    }

    #[test]
    fn the_lexer_knows_duckdbs_richer_quoting() {
        // Tagged dollar-quotes: the body runs to the EXACT delimiter,
        // so a ';' inside — even a runnable one — never divides.
        assert_eq!(
            split("SELECT $q$x; DROP TABLE t; y$q$;"),
            ["SELECT $q$x; DROP TABLE t; y$q$;"]
        );
        // A parameter is not a quote.
        assert_eq!(split("SELECT $1; SELECT 2;"), ["SELECT $1;", "SELECT 2;"]);
        // Block comments nest (Postgres heritage): the first */ closes
        // the INNER comment, not the outer one.
        assert_eq!(
            split("/* outer /* inner */ ; */ SELECT 1;"),
            ["/* outer /* inner */ ; */ SELECT 1;"]
        );
        // e-strings escape with backslashes: the \' is content.
        assert_eq!(split(r"SELECT e'\';' ;"), [r"SELECT e'\';' ;"]);
    }

    #[test]
    fn a_statement_is_paged_by_its_first_word_past_any_comment() {
        // Paged by wrapping: the SELECT family, and the statements that
        // answer with a table of their own, behind either kind of comment.
        for sql in [
            "select 1", "WITH a AS (SELECT 1) SELECT * FROM a", "from t", "VALUES (1)", "TABLE t",
            "(SELECT 1)", "/* note */ SELECT * FROM big", "-- note\n/* and */ from big",
            "/* a /* nested */ b */ (SELECT 1)", "-- to the carriage return\rSELECT 1",
            "\u{200b}SELECT 1", "DESCRIBE t", "desc t", "SUMMARIZE t", "SHOW TABLES",
            "PIVOT t ON a USING sum(b)", "UNPIVOT t ON a INTO NAME k VALUE v",
        ] {
            assert!(wrappable(sql), "{sql}");
        }
        for sql in [
            "INSERT INTO t VALUES (1)", "BEGIN", "/* select */ UPDATE t SET a = 1", "EXPLAIN SELECT 1",
            "EXPLAIN ANALYZE SELECT 1", "", "/* select", "-- select", "\"select\" 1", "select_all",
        ] {
            assert!(!wrappable(sql), "{sql}");
        }
    }

    #[test]
    fn a_transaction_statement_is_read_as_the_engine_reads_it() {
        use TxnEffect::{Commits, Opens, RollsBack};
        for (sql, effect) in [
            ("BEGIN", Opens),
            ("begin transaction", Opens),
            ("  Begin;", Opens),
            ("START TRANSACTION", Opens),
            ("-- go\nBEGIN", Opens),
            ("/* go */ begin", Opens),
            ("EXPLAIN ANALYZE BEGIN", Opens),
            ("EXPLAIN (ANALYZE) BEGIN", Opens),
            ("COMMIT", Commits),
            ("END", Commits),
            ("END TRANSACTION", Commits),
            ("COMMIT--x", Commits),
            ("-- c\rCOMMIT", Commits),
            ("EXPLAIN ANALYZE COMMIT", Commits),
            ("EXPLAIN ANALYSE COMMIT", Commits),
            ("EXPLAIN (ANALYZE false) COMMIT", Commits),
            ("EXPLAIN (FORMAT JSON, ANALYZE) COMMIT", Commits),
            ("EXPLAIN ANALYZE (FORMAT JSON) COMMIT", Commits),
            ("ROLLBACK", RollsBack),
            ("abort", RollsBack),
            ("EXPLAIN ANALYZE ROLLBACK", RollsBack),
            // The spaces the engine skips, a zero-width one pasted from the
            // web among them: the engine commits, so the view must know it.
            ("\u{200b}COMMIT", Commits),
            ("\u{feff}COMMIT", Commits),
            ("\u{2060}ROLLBACK", RollsBack),
            ("\u{a0}BEGIN", Opens),
            ("COMMIT\u{200b}", Commits),
        ] {
            assert_eq!(txn_effect(sql), Some(effect), "{sql:?}");
        }
        // Each of these is a name to the engine, plans without running, or
        // sits behind a character the engine does not skip.
        for sql in [
            "SELECT 'BEGIN'", "-- BEGIN\nSELECT 1", "/* COMMIT */ UPDATE t SET a = 1", "BEGINNING", "",
            "COMMIT_X", "COMMIT1", "COMMIT$x", "COMMITé", "\"COMMIT\"", "\"BEGIN\"", "(COMMIT)",
            "EXPLAIN COMMIT", "EXPLAIN (FORMAT JSON) COMMIT", "EXPLAIN (FORMAT JSON) ANALYZE COMMIT",
            "\u{85}COMMIT", "\u{1680}COMMIT", "\u{2028}COMMIT", "\u{2029}BEGIN",
        ] {
            assert_eq!(txn_effect(sql), None, "{sql:?}");
        }
        // One reader for the view, the server and Harbor's own client: the
        // view's effect is the shared one's, split by what ends it.
        for sql in ["BEGIN", "\u{200b}COMMIT", "EXPLAIN ANALYZE ROLLBACK", "END", "ABORT", "SELECT 1", "\u{85}COMMIT", "\"BEGIN\""] {
            assert_eq!(
                txn_effect(sql).map(|effect| effect == Opens),
                wire::statement::transaction_effect(sql),
                "{sql:?}"
            );
        }
    }

    #[test]
    fn a_statement_runs_alone_on_the_open_session_or_on_one_opened_for_it() {
        let effects = [None, Some(TxnEffect::Opens), Some(TxnEffect::Commits), Some(TxnEffect::RollsBack)];
        // No transaction: only a statement that begins one gets a session.
        // COMMIT and ROLLBACK go alone, and the engine says there is none.
        assert_eq!(route(false, false, Some(TxnEffect::Opens)), Route::Opening);
        for effect in [None, Some(TxnEffect::Commits), Some(TxnEffect::RollsBack)] {
            assert_eq!(route(false, false, effect), Route::Alone, "{effect:?}");
        }
        // A transaction open: everything goes to its session, a second
        // BEGIN included, which the engine refuses there.
        for effect in effects {
            assert_eq!(route(true, false, effect), Route::Held, "{effect:?}");
            assert_eq!(route(true, true, effect), Route::Held, "{effect:?}");
        }
    }

    #[test]
    fn the_run_after_a_lost_transaction_is_refused_once() {
        // The keepalive found the session gone and raised the latch. Whatever
        // is run next was typed for that transaction and is not sent: alone,
        // an UPDATE would commit on its own.
        for effect in [None, Some(TxnEffect::Opens), Some(TxnEffect::Commits), Some(TxnEffect::RollsBack)] {
            assert_eq!(route(false, true, effect), Route::Refused, "{effect:?}");
            assert_eq!(fate(Route::Refused, effect, false, None), Fate::Closed, "{effect:?}");
        }
        // The run reads the latch and lowers it, as `QueryView::run` does, so
        // the same statement sent again goes ahead on its own.
        let mut lost = true;
        let first = route(false, std::mem::take(&mut lost), None);
        let second = route(false, std::mem::take(&mut lost), None);
        assert_eq!((first, second), (Route::Refused, Route::Alone));
    }

    fn refused(message: &str) -> Failure {
        Failure::Refused { code: "sql_error".into(), message: message.into() }
    }

    fn harbor(code: &str) -> Failure {
        Failure::Refused { code: code.into(), message: format!("harbor says {code}") }
    }

    #[test]
    fn a_failed_statement_ran_did_not_or_may_have() {
        // The engine ran it and refused it: every class but the parser's.
        for message in [
            "Catalog Error: Table with name nope does not exist!",
            "Binder Error: Referenced column \"nocol\" not found in FROM clause!",
            "Constraint Error: Duplicate key \"id: 1\" violates primary key constraint.",
            "Conversion Error: Could not convert string 'abc' to INT32",
            "Out of Range Error: Overflow in addition of INT32 (2147483647 + 1)!",
            "Invalid Input Error: boom",
            "TransactionContext Error: cannot start a transaction within a transaction",
            "TransactionContext Error: Current transaction is aborted (please ROLLBACK)",
        ] {
            assert_eq!(ran(&refused(message)), Ran::Failed, "{message}");
        }
        // It never ran: the parser refused it, Harbor refused it before the
        // engine saw it, or it could not be sent.
        assert_eq!(ran(&refused("Parser Error: syntax error at or near \"foo\"")), Ran::Never);
        for code in [
            "bad_request", "not_found", "forbidden", "body_too_large", "no_such_session", "session_busy",
            "query_id_in_use", "no_lease_connections", "no_lease_available", "unavailable", "unready",
        ] {
            assert_eq!(ran(&harbor(code)), Ran::Never, "{code}");
        }
        assert_eq!(ran(&Failure::Unsent("query: Connection refused (os error 61)".into())), Ran::Never);
        // It may have run: Harbor's answers about a statement the engine had
        // begun, a code this client does not know, and silence.
        for code in ["cancelled", "internal", "response_too_large", "unsupported_type", "some_later_code"] {
            assert_eq!(ran(&harbor(code)), Ran::Unknown, "{code}");
            assert!(!never_ran(&harbor(code)), "{code}");
        }
        assert_eq!(ran(&Failure::Unanswered("query: timed out".into())), Ran::Unknown);
    }

    #[test]
    fn only_the_sessions_own_answer_says_whether_it_is_aborted() {
        let ok = harbor_client::QueryResult { columns: vec![], rows: vec![], row_count: 1, time_ms: 0 };
        assert_eq!(asked(Ok(ok)), Asked::Is(Health::Fine));
        let aborted = refused("TransactionContext Error: Current transaction is aborted (please ROLLBACK)");
        assert_eq!(asked(Err(aborted)), Asked::Is(Health::Aborted));
        let gone = harbor("no_such_session");
        assert_eq!(asked(Err(gone.clone())), Asked::Gone(gone));
        // Busy with the statement before, or no answer within the wait: no
        // verdict, and nothing known before is changed by it.
        for failure in [
            harbor("session_busy"),
            Failure::Unanswered("query: Resource temporarily unavailable (os error 35)".into()),
            Failure::Unsent("query: Connection refused (os error 61)".into()),
            refused("Binder Error: something else entirely"),
        ] {
            assert!(matches!(asked(Err(failure.clone())), Asked::Unanswered(_)), "{failure:?}");
        }

        // The band says which of the three it is.
        let left = std::time::Duration::from_secs(272);
        assert_eq!(
            transaction_mark(Health::Fine, left),
            "transaction open \u{b7} 4:32 left \u{b7} COMMIT or ROLLBACK ends it"
        );
        assert_eq!(
            transaction_mark(Health::Aborted, left),
            "transaction aborted by an error \u{b7} 4:32 left \u{b7} ROLLBACK ends it"
        );
        assert!(transaction_mark(Health::Unknown, left).starts_with("transaction open, its state unconfirmed"));
    }

    #[test]
    fn a_commit_is_sent_only_when_the_sessions_state_is_confirmed() {
        // Confirmed sound, it is sent, and will commit.
        assert_eq!(commit_gate(Asked::Is(Health::Fine)), Ok(false));
        // Confirmed aborted, it is sent too, and its answer is read as the
        // rollback it is.
        assert_eq!(commit_gate(Asked::Is(Health::Aborted)), Ok(true));
        // Unconfirmed, it is not sent: it could roll everything back and
        // answer like a commit. The failure says so, and is one of a
        // statement that never ran, so the transaction stays open.
        let unconfirmed = commit_gate(Asked::Unanswered("session_busy: busy".into())).unwrap_err();
        assert!(matches!(&unconfirmed, Failure::Unsent(why)
            if why.starts_with("could not confirm the transaction's state (session_busy: busy)")
                && why.ends_with("try again")));
        assert_eq!(fate(Route::Held, Some(TxnEffect::Commits), false, Some(&unconfirmed)), Fate::Open);
        // A session that is gone is reported as that.
        let gone = commit_gate(Asked::Gone(harbor("no_such_session"))).unwrap_err();
        assert_eq!(fate(Route::Held, Some(TxnEffect::Commits), false, Some(&gone)), Fate::Lost);
    }

    #[test]
    fn a_commit_of_an_aborted_transaction_is_reported_as_the_rollback_it_is() {
        let (commits, rolls_back) = (Some(TxnEffect::Commits), Some(TxnEffect::RollsBack));
        // The reviewer's case: BEGIN; INSERT; a SELECT whose count hits a
        // Conversion Error; COMMIT. The COMMIT answers, and rolled back.
        assert_eq!(fate(Route::Held, commits, true, None), Fate::RolledBack);
        // Not aborted, it committed.
        assert_eq!(fate(Route::Held, commits, false, None), Fate::Closed);
        // With no verdict on the COMMIT there is still no doubt: aborted, it
        // rolls back when it runs or when its session is released.
        for failure in [Failure::Unanswered("query: timed out".into()), harbor("cancelled"), harbor("internal")] {
            assert_eq!(fate(Route::Held, commits, true, Some(&failure)), Fate::RolledBack, "{failure:?}");
        }
        // Not aborted: with no answer, or Harbor's `internal`, it may have
        // committed; answered `cancelled`, it never started.
        for failure in [Failure::Unanswered("query: timed out".into()), harbor("internal")] {
            assert_eq!(fate(Route::Held, commits, false, Some(&failure)), Fate::InDoubt, "{failure:?}");
        }
        assert_eq!(fate(Route::Held, commits, false, Some(&harbor("cancelled"))), Fate::NotKept);
        assert!(Fate::NotKept.note(commits).unwrap().starts_with("The COMMIT did not run, and nothing since BEGIN was kept."));
        // A ROLLBACK does what was asked either way, and says no more.
        assert_eq!(fate(Route::Held, rolls_back, true, None), Fate::Closed);
        assert_eq!(fate(Route::Held, rolls_back, false, None), Fate::Closed);
        // Outside a held transaction the flag means nothing.
        assert_eq!(fate(Route::Alone, commits, true, None), Fate::Closed);
        // The view's own message stands in for "ok", and needs no note.
        assert_eq!(Fate::RolledBack.note(commits), None);
        assert!(super::ROLLED_BACK.contains("rolled back: an earlier error aborted the transaction"));
        assert!(super::ROLLED_BACK_UNANSWERED.contains("rolled back either way"));
    }

    #[test]
    fn the_transaction_after_a_statement_follows_its_verdict() {
        let (opens, commits, rolls_back) =
            (Some(TxnEffect::Opens), Some(TxnEffect::Commits), Some(TxnEffect::RollsBack));
        let parse = refused("Parser Error: syntax error at or near \"foo\"");
        let conflict = refused("TransactionContext Error: Failed to commit: PRIMARY KEY or UNIQUE constraint violation");
        let catalog = refused("Catalog Error: Table with name nope does not exist!");
        let gone = Failure::Refused { code: "no_such_session".into(), message: "no such session".into() };
        let busy = Failure::Refused {
            code: "session_busy".into(),
            message: "this session is already running a statement".into(),
        };
        let unavailable = Failure::Refused { code: "unavailable".into(), message: "harbor is not serving".into() };
        let unsent = Failure::Unsent("query: Connection refused (os error 61)".into());
        let lost_answer = Failure::Unanswered("query: Resource temporarily unavailable (os error 35)".into());

        // Alone, nothing is open before or after, whatever the verdict.
        for failure in [None, Some(&catalog), Some(&lost_answer)] {
            assert_eq!(fate(Route::Alone, commits, false, failure), Fate::Closed);
        }
        for failure in [None, Some(&catalog), Some(&parse), Some(&unsent)] {
            assert_eq!(fate(Route::Alone, None, false, failure), Fate::Closed);
        }
        // A statement alone that got no answer may have run, and committed:
        // the view says so, and a rerun is the user's to weigh.
        for failure in [&lost_answer, &harbor("cancelled"), &harbor("internal")] {
            assert_eq!(fate(Route::Alone, None, false, Some(failure)), Fate::MaybeRan, "{failure:?}");
        }
        assert!(Fate::MaybeRan.note(None).unwrap().contains("may have run, and on its own it commits"));
        // BEGIN opens one only if it succeeded; its session goes back otherwise.
        assert_eq!(fate(Route::Opening, opens, false, None), Fate::Open);
        assert_eq!(fate(Route::Opening, opens, false, Some(&catalog)), Fate::Closed);
        assert_eq!(fate(Route::Opening, opens, false, Some(&lost_answer)), Fate::Closed);

        // Inside one, an ordinary statement leaves it open, failed or not:
        // an aborted transaction is still open until ROLLBACK or COMMIT.
        for failure in [None, Some(&parse), Some(&catalog), Some(&busy), Some(&unsent), Some(&lost_answer)] {
            assert_eq!(fate(Route::Held, None, false, failure), Fate::Open);
            assert_eq!(fate(Route::Held, opens, false, failure), Fate::Open);
        }
        for ends in [commits, rolls_back] {
            // COMMIT and ROLLBACK end it. One that did not parse never ran;
            // one the engine refused ended it rolled back; one with no
            // answer may have gone either way.
            assert_eq!(fate(Route::Held, ends, false, None), Fate::Closed);
            assert_eq!(fate(Route::Held, ends, false, Some(&parse)), Fate::Open);
            assert_eq!(fate(Route::Held, ends, false, Some(&conflict)), Fate::Failed);
            assert_eq!(fate(Route::Held, ends, false, Some(&lost_answer)), Fate::InDoubt);
            // One Harbor refused before the engine saw it, or that could not
            // be sent, leaves the transaction open and the session held: the
            // statement before it is still running there, or the server is
            // not serving. Releasing the session would cancel that statement
            // and roll everything back.
            for failure in [&busy, &unavailable, &unsent] {
                assert_eq!(fate(Route::Held, ends, false, Some(failure)), Fate::Open, "{failure:?}");
            }
            // One Harbor interrupted after the engine had it, at a deadline
            // or a cancel, or failed on after it ran, has no verdict either:
            // the transaction is not shown as open.
            assert_eq!(fate(Route::Held, ends, false, Some(&harbor("internal"))), Fate::InDoubt);
            let cancelled = if ends == commits { Fate::NotKept } else { Fate::InDoubt };
            assert_eq!(fate(Route::Held, ends, false, Some(&harbor("cancelled"))), cancelled);
            // The engine found no transaction to end: the view's mark was
            // wrong, and nothing is claimed to have been rolled back.
            for verb in ["commit", "rollback"] {
                let none = refused(&format!("TransactionContext Error: cannot {verb} - no transaction is active"));
                assert_eq!(fate(Route::Held, ends, false, Some(&none)), Fate::NoneActive);
            }
        }
        // A session the server reclaimed is gone for every statement, and
        // none of them is then run outside it.
        for effect in [None, opens, commits, rolls_back] {
            assert_eq!(fate(Route::Held, effect, false, Some(&gone)), Fate::Lost, "{effect:?}");
        }

        // Only an outcome the engine's message leaves unsaid earns a note.
        assert_eq!(Fate::Open.note(None), None);
        assert_eq!(Fate::Closed.note(commits), None);
        assert!(Fate::Failed.note(commits).unwrap().contains("rolled back"));
        let none = Fate::NoneActive.note(rolls_back).unwrap();
        assert!(none.contains("held no transaction") && none.contains("changed nothing"));
        assert!(!none.contains("rolled back"), "{none}");
        // An unanswered COMMIT may have gone either way; an unanswered
        // ROLLBACK rolled back either way.
        assert!(Fate::InDoubt.note(commits).unwrap().contains("either way"));
        let rollback = Fate::InDoubt.note(rolls_back).unwrap();
        assert!(rollback.contains("rolls the transaction back") && !rollback.contains("either way"));
        assert!(Fate::Lost.note(None).unwrap().ends_with("This statement did not run."));
        assert!(Fate::Lost.note(commits).unwrap().ends_with("everything since BEGIN."));
    }

    /// A transaction's session goes back when its last holder is dropped
    /// without a statement having ended it (the view closed, the connection
    /// dropped, another database was chosen), and the server rolls it back.
    /// Live: needs `HARBOR_LIVE_DB`, a scratch database file, as the probes
    /// in harbor-client's `tests/live.rs` do.
    #[test]
    #[ignore]
    fn a_dropped_transaction_gives_its_session_back() {
        let Some(db) = std::env::var_os("HARBOR_LIVE_DB") else {
            println!("set HARBOR_LIVE_DB to a scratch database file; skipping");
            return;
        };
        let conn = harbor_client::fleet::connect_path(std::path::Path::new(&db)).expect("connect");
        let alone = |sql: &str| harbor_client::exec_checked(&conn, sql, None, None);
        alone("CREATE OR REPLACE TABLE _dt_drop_probe(v VARCHAR)").expect("create");
        alone("INSERT INTO _dt_drop_probe VALUES ('a')").expect("insert");

        let txn = super::Txn::open(&conn).expect("session");
        let results_grid = txn.clone();
        let id = txn.0.session.id.clone();
        txn.exec("BEGIN").expect("BEGIN");
        txn.exec("UPDATE _dt_drop_probe SET v = 'b'").expect("update");
        assert_eq!(txn.touch(), super::Touch::Alive);
        // One holder left: the session is still held.
        drop(txn);
        assert!(harbor_client::exec_checked(&conn, "SELECT 1", None, Some(&id)).is_ok());
        // The last holder gone: released off-thread, so give it a moment.
        drop(results_grid);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let gone = loop {
            match harbor_client::exec_checked(&conn, "SELECT 1", None, Some(&id)) {
                Err(failure) if failure.session_gone() => break true,
                _ if std::time::Instant::now() > deadline => break false,
                _ => std::thread::sleep(std::time::Duration::from_millis(50)),
            }
        };
        assert!(gone, "the session outlived its transaction's last holder");
        let read = alone("SELECT v FROM _dt_drop_probe").expect("select");
        assert_eq!(read.rows[0][0], serde_json::json!("a"), "and the transaction was rolled back");
        // The server names the session gone before its rollback has finished,
        // and until then a write to the same row conflicts with it.
        while alone("UPDATE _dt_drop_probe SET v = v").is_err() {
            assert!(std::time::Instant::now() < deadline, "the released transaction never rolled back");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        // Ended by a statement, the release happens once, and a page asked
        // of a transaction that is over is read like any other.
        let txn = super::Txn::open(&conn).expect("session");
        txn.exec("BEGIN").expect("BEGIN");
        txn.exec("UPDATE _dt_drop_probe SET v = 'c'").expect("update");
        txn.exec("COMMIT").expect("COMMIT");
        txn.release();
        assert_eq!(txn.touch(), super::Touch::Gone);

        // A session the server no longer knows is marked over by the touch
        // that finds it gone, so a results grid holding it pages the
        // ordinary way and not against the dead session.
        let txn = super::Txn::open(&conn).expect("session");
        txn.exec("BEGIN").expect("BEGIN");
        harbor_client::session_release(&conn, &txn.0.session.id);
        assert!(!txn.over());
        assert_eq!(txn.touch(), super::Touch::Gone);
        assert!(txn.over());
        let page = txn.page("SELECT v FROM _dt_drop_probe").expect("a page after the session is gone");
        assert_eq!(page.rows[0][0], serde_json::json!("c"));
        // A page asked before any touch has found the loss finds it itself.
        let txn = super::Txn::open(&conn).expect("session");
        txn.exec("BEGIN").expect("BEGIN");
        harbor_client::session_release(&conn, &txn.0.session.id);
        let page = txn.page("SELECT v FROM _dt_drop_probe").expect("a page from a session just lost");
        assert_eq!(page.rows[0][0], serde_json::json!("c"));
        assert!(txn.over());
        let page = txn.page("SELECT v FROM _dt_drop_probe").expect("a page after the transaction");
        assert_eq!(page.rows[0][0], serde_json::json!("c"));
        alone("DROP TABLE _dt_drop_probe").expect("drop");
    }

    /// The COMMIT that reports a rollback (docs/QUERY.md): an error aborts
    /// the transaction, the session says so when asked, the COMMIT answers
    /// like any other, and nothing of the transaction is kept. Live: needs
    /// `HARBOR_LIVE_DB`, a scratch database file.
    #[test]
    #[ignore]
    fn a_commit_after_an_error_is_known_to_roll_back() {
        let Some(db) = std::env::var_os("HARBOR_LIVE_DB") else {
            println!("set HARBOR_LIVE_DB to a scratch database file; skipping");
            return;
        };
        let conn = harbor_client::fleet::connect_path(std::path::Path::new(&db)).expect("connect");
        let alone = |sql: &str| harbor_client::exec_checked(&conn, sql, None, None);
        alone("CREATE OR REPLACE TABLE _dt_aborted_probe(id INTEGER, v VARCHAR)").expect("create");
        alone("INSERT INTO _dt_aborted_probe VALUES (1, '1'), (2, 'x')").expect("insert");
        let rows = || alone("SELECT count(*) FROM _dt_aborted_probe").expect("count").rows[0][0].clone();
        let commits = Some(TxnEffect::Commits);

        // BEGIN; INSERT; a count that reads every row and meets one that
        // does not convert. A first page of the same statement would not.
        let txn = super::Txn::open(&conn).expect("session");
        txn.exec("BEGIN").expect("BEGIN");
        txn.exec("INSERT INTO _dt_aborted_probe VALUES (3, '3')").expect("insert");
        assert_eq!(txn.health(), Health::Fine);
        let page = "SELECT * FROM (\nSELECT v::INTEGER FROM _dt_aborted_probe ORDER BY id\n) LIMIT 1 OFFSET 0";
        txn.exec(page).expect("the first page reads one row, which converts");
        assert_eq!(txn.health(), Health::Fine);
        let count = "SELECT count(*) FROM (\nSELECT v::INTEGER AS n FROM _dt_aborted_probe ORDER BY id\n) WHERE n > 0";
        let failure = txn.exec(count).unwrap_err();
        println!("the count: {failure}");
        assert_eq!(txn.health(), Health::Aborted, "a Conversion Error aborts, and the band says so at once");
        // A statement that answers on an aborted transaction proves nothing:
        // the band still says aborted after it.
        let prepared = txn.exec("PREPARE _dt_p AS SELECT 1");
        println!("PREPARE on the aborted transaction: {:?}", prepared.as_ref().map(|_| "answered").map_err(ToString::to_string));
        assert_eq!(txn.health(), Health::Aborted);
        // The COMMIT is sent in the same turn as the question, answers like
        // any other, and what it did is roll back.
        let (aborted, answer) = txn.commit("COMMIT");
        assert!(aborted);
        answer.expect("COMMIT answers");
        assert_eq!(fate(Route::Held, commits, aborted, None), Fate::RolledBack);
        txn.release();
        assert_eq!(rows(), serde_json::json!(2), "the INSERT is gone");

        // A results page read inside the transaction aborts it the same way.
        let txn = super::Txn::open(&conn).expect("session");
        txn.exec("BEGIN").expect("BEGIN");
        txn.exec("INSERT INTO _dt_aborted_probe VALUES (3, '3')").expect("insert");
        assert!(txn.page("SELECT * FROM _dt_no_such_table LIMIT 5 OFFSET 5").is_err());
        assert_eq!(txn.health(), Health::Aborted);
        txn.exec("ROLLBACK").expect("ROLLBACK");
        txn.release();

        // A parse error leaves it as it was. Harbor's own refusal of a
        // protected setting reads like an engine error, so the session is
        // asked in the same turn and the band never says aborted of it; the
        // COMMIT that follows is known to have landed.
        let txn = super::Txn::open(&conn).expect("session");
        txn.exec("BEGIN").expect("BEGIN");
        txn.exec("INSERT INTO _dt_aborted_probe VALUES (3, '3')").expect("insert");
        assert!(txn.exec("SELEC 1").is_err());
        assert_eq!(txn.health(), Health::Fine);
        let refused = txn.exec("SET memory_limit = '1GB'").unwrap_err();
        println!("a protected setting: {refused}");
        assert_eq!(txn.health(), Health::Fine, "asked at once, and found sound");
        let (aborted, answer) = txn.commit("COMMIT");
        assert!(!aborted);
        answer.expect("COMMIT");
        assert_eq!(fate(Route::Held, commits, aborted, None), Fate::Closed);
        txn.release();
        assert_eq!(rows(), serde_json::json!(3), "this one committed");

        // A COMMIT on a session still busy with another statement cannot be
        // confirmed, and is not sent: the transaction stays as it was.
        let txn = super::Txn::open(&conn).expect("session");
        txn.exec("BEGIN").expect("BEGIN");
        txn.exec("INSERT INTO _dt_aborted_probe VALUES (4, '4')").expect("insert");
        let id = txn.0.session.id.clone();
        let (busy_conn, busy_id) = (conn.clone(), id.clone());
        let slow = std::thread::spawn(move || {
            harbor_client::exec_checked(
                &busy_conn,
                "SELECT count(*) FROM range(3000000000) t(i) WHERE i % 7 = 3",
                None,
                Some(&busy_id),
            )
        });
        std::thread::sleep(std::time::Duration::from_millis(400));
        let (aborted, answer) = txn.commit("COMMIT");
        let unconfirmed = answer.unwrap_err();
        println!("COMMIT on a busy session: {unconfirmed}");
        assert!(!aborted && matches!(&unconfirmed, harbor_client::Failure::Unsent(_)));
        assert_eq!(fate(Route::Held, commits, aborted, Some(&unconfirmed)), Fate::Open);
        assert_eq!(rows(), serde_json::json!(3), "nothing was committed");
        slow.join().expect("the slow statement's thread").expect("it finished");
        let (aborted, answer) = txn.commit("COMMIT");
        answer.expect("COMMIT, once the session is free");
        assert!(!aborted);
        txn.release();
        assert_eq!(rows(), serde_json::json!(4));

        // The keepalive learns of an abort it did not cause, within its wait.
        let txn = super::Txn::open(&conn).expect("session");
        txn.exec("BEGIN").expect("BEGIN");
        let id = txn.0.session.id.clone();
        assert!(harbor_client::exec_checked(&conn, "SELECT * FROM _dt_no_such_table", None, Some(&id)).is_err());
        assert_eq!(txn.health(), Health::Fine);
        let began = std::time::Instant::now();
        assert_eq!(txn.touch(), super::Touch::Alive);
        assert_eq!(txn.health(), Health::Aborted);
        assert!(began.elapsed() < std::time::Duration::from_secs(5));
        txn.release();
        alone("DROP TABLE _dt_aborted_probe").expect("drop");
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
        assert_eq!(
            split(text),
            ["SELECT 'a;b'; -- c;", "SELECT 2;", "/* ; */ SELECT 3"]
        );
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
            "$$ ; ", "é;∅", "\u{1F986}", "", "$tag$;x$tag$", "$t$",
            "$1;", r"e'\';'", "e'", r"\", r"e'\é'", "/* /* ; */",
            "; -- t;\n", "; --",
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
            let stmts = split_statements(&text);
            let mut prev_end = 0;
            for st in &stmts {
                let s = &st.span;
                // In bounds, ordered, disjoint, on char boundaries.
                assert!(prev_end <= s.start && s.start < s.end && s.end <= text.len());
                let body = text.get(s.clone()).expect("span on char boundary");
                prev_end = s.end;
                // Spans are their trimmed selves, and never terminator-only.
                assert_eq!(body, body.trim());
                assert!(!body.chars().all(|c| c == ';'));
                // The payload is the span's head: same start, trimmed,
                // ending before the terminator (a deeper `;` is
                // statement text — the fuzzer's first scalp was a
                // strip-them-all implementation).
                assert!(st.payload.start == s.start && st.payload.end <= s.end);
                let payload = text.get(st.payload.clone()).expect("payload on char boundary");
                assert_eq!(payload, payload.trim_end());
                if st.terminated {
                    assert!(
                        text[st.payload.end..s.end].trim_start().starts_with(';'),
                        "terminated {body:?} has no `;` after its payload"
                    );
                }
                // Statements begin at top level, so a statement's own
                // text re-splits to exactly itself: no interior
                // top-level `;` can be hiding in a span, and the
                // payload/terminator carve is position-independent.
                let again = split_statements(body);
                assert_eq!(again.len(), 1, "re-split of {body:?}");
                assert_eq!(again[0].span, 0..body.len(), "re-split of {body:?}");
                assert_eq!(again[0].terminated, st.terminated, "re-split of {body:?} in {text:?}");
                assert_eq!(
                    again[0].payload,
                    (st.payload.start - s.start)..(st.payload.end - s.start)
                );
            }
            // Everything OUTSIDE the spans is gap: whitespace and
            // dropped terminators only — no statement text ever leaks.
            let mut outside = String::new();
            let mut at = 0;
            for st in &stmts {
                outside.push_str(&text[at..st.span.start]);
                // The terminator belongs to its statement: stray `;`s
                // may trail a CLOSED span (a dropped `;;`), but an
                // open span abandoning its own terminator outside is
                // exactly the bug this hunts.
                if !st.terminated {
                    assert!(
                        !text[st.span.end..].trim_start().starts_with(';'),
                        "span {:?} stranded its terminator in {text:?}",
                        &text[st.span.clone()]
                    );
                }
                at = st.span.end;
            }
            outside.push_str(&text[at..]);
            assert!(
                outside.chars().all(|c| c.is_whitespace() || c == ';'),
                "leaked {outside:?} from {text:?}"
            );
            // Every caret owns one of the spans (or none when there are
            // none), and the payload readers agree with the lex.
            for caret in [0, text.len() / 2, text.len(), text.len() + 7] {
                let span = statement_span(&text, caret);
                assert_eq!(span.is_none(), stmts.is_empty());
                if let Some(r) = span {
                    let st = stmts.iter().find(|s| s.span == r).expect("span from the lex");
                    let want = (!st.payload.is_empty())
                        .then(|| text[st.payload.clone()].to_string());
                    assert_eq!(statement_at(&text, caret), want);
                }
            }
        }
    }
}
