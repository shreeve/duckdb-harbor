//! The results grid: gpui-component's virtualized Table underneath (its
//! two-axis virtualization stress-checked by `examples/wide_probe.rs`, a
//! plain-cell probe of the library — not of this delegate), DuckTable's
//! delegate on top.
//! This surface owns fetching (server-side pages via `POST /sql`), value
//! presentation, and its header/status strips. Editing layers on later
//! (`edits.rs`); display ships first.
//!
//! Rows arrive as explicit pages: a fetched page REPLACES the rows in one
//! frame (DESIGN.md: fetch first, commit over the old value), so the grid
//! always shows one internally consistent snapshot. Row indices here are
//! display positions within the current page — the moment sorting or
//! editing arrives, reads resolve through an identity mapping, never raw
//! indices.

use crate::chrome::{icon_tile, toggle_tile};
use crate::edits::{self, Edits};
use crate::prefs::{self, ViewMode};
use crate::theme::{
    pal, ui_font, value_font, CELL_TEXT, GUTTER_TEXT, HEADER_TEXT, PANE_INSET, TAG_TEXT,
};
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::table::{Column as TableColumn, Table, TableDelegate, TableState};
use gpui_component::tooltip::Tooltip;
use gpui_component::resizable::{
    h_resizable, resizable_panel, ResizablePanelEvent, ResizableState,
};
use gpui_component::{Sizable as _, StyledExt as _};
use harbor_client::Conn;
use serde_json::Value;

// Page sizes live in prefs::PAGE_SIZES (default 500); explicit pages give
// ordinary tables a boundary-free read and huge tables honest jumps,
// constant memory, and consistent snapshots — infinite append would
// silently stitch separately-queried chunks together.

// 7 is Menlo's digit advance at GUTTER_TEXT (11px); 16 is the gutter's
// horizontal padding. Both move if the value font or size does.
fn gutter_width(max_row: u64) -> f32 {
    let digits = max_row.max(1).ilog10() as f32 + 1.;
    (16. + digits * 7.).max(34.)
}

pub(crate) struct Grid {
    // `table`, `filter_input`, and `col_search` are pub(crate) for the
    // satellite `impl Grid` files (footer.rs); nothing outside those
    // renders should touch them.
    pub(crate) table: Entity<TableState<GridDelegate>>,
    conn: Conn,
    /// Quoted `"schema"."table"` this grid pages from.
    source: String,
    title: String,
    /// Current page (0-based) and the size its rows were fetched with.
    pub(crate) page: usize,
    pub(crate) page_size: usize,
    /// Raw SQL WHERE clause text, verbatim from the filter strip.
    pub(crate) filter: Option<String>,
    /// Exact server-side row count (under the current filter), when the
    /// count query succeeded.
    pub(crate) total_rows: Option<u64>,
    error: Option<String>,
    pub(crate) last_time_ms: u64,
    /// The staging layer (docs/EDITING.md), present only when the table
    /// is editable — it has a primary key. None = read-only.
    pub(crate) edits: Option<Edits>,
    /// The open cell editor, if any. Provisional input lives here; it
    /// becomes a staged change only on confirm.
    editor: Option<CellEditor>,
    /// Primary-key column names from the catalog — kept so an error-born
    /// grid can build its Edits when its first schema finally lands.
    pk_cols: Vec<String>,
    /// NOT NULL per schema column (from the catalog, by name) — staging
    /// NULL into one refuses at the fingers, not at the server.
    not_null: Vec<bool>,
    /// A commit is in flight; ⌘S is a no-op until it resolves.
    pub(crate) committing: bool,
    /// Focus should return to the table on the next frame — set by paths
    /// that lack a Window (subscriptions), consumed by render.
    needs_focus: bool,
    /// The ring's seat across a keyboard page flip (page_step): the
    /// fetch that lands consumes it, row clamped to the new page.
    ring_keep: Option<(usize, usize)>,
    /// The filter strip's input; Some = the strip is open.
    pub(crate) filter_input: Option<Entity<gpui_component::input::InputState>>,
    /// Prefetched with the first page, so switching views is instant.
    structure: Option<crate::structure::TableStructure>,
    /// The table/inspector divider (UI.md: divider positions persist —
    /// the width saves at the end of each drag).
    resize: Entity<ResizableState>,
    /// The Columns popover's search box — persistent so the query
    /// survives re-renders while the popover is open.
    pub(crate) col_search: Entity<gpui_component::input::InputState>,
    /// The Structure view's DDL block: a DISABLED multi-line Input, so
    /// the text is natively selectable (mouse drag, Cmd+C) while every
    /// mutation stays gated off. None when the table has no DDL.
    pub(crate) ddl_input: Option<Entity<gpui_component::input::InputState>>,
    /// The DDL block's copy tile, a self-confirming widget (copy_button.rs).
    pub(crate) ddl_copy: Option<Entity<crate::copy_button::CopyButton>>,
    /// Fence for page fetches: a newer fetch supersedes an older one in
    /// flight, whose outcome is then discarded instead of committing
    /// stale rows.
    fetch_seq: u64,
    /// The table wrapper's window bounds, recorded by a canvas each frame
    /// so `divider_double_click` can hit-test header dividers.
    table_bounds: std::rc::Rc<std::cell::Cell<Bounds<Pixels>>>,
}

/// Everything a fetch commits along with its rows. The delegate is not
/// touched until the data arrives — the footer, funnel, and gutter always
/// describe the rows actually on screen, and a failed or superseded fetch
/// leaves no half-applied state behind.
struct PageReq {
    page: usize,
    size: usize,
    filter: FilterChange,
    recount: bool,
}

/// One open cell editor. Entry gesture decides arrow physics (docs/
/// EDITING.md): replace entry (typed) — arrows confirm and move the
/// ring; kept-value entry (Enter/double-click) — arrows move the caret.
struct CellEditor {
    row: usize,
    /// Schema column index.
    col: usize,
    input: Entity<gpui_component::input::InputState>,
    replace: bool,
}

/// What a fetch does to the WHERE filter.
enum FilterChange {
    /// Keep the current filter.
    Keep,
    /// Commit a new one with the rows (None clears it).
    Set(Option<String>),
}

/// What the Table renders from, per cell per frame — and nothing else.
/// The query/session state (page, filter, totals, errors) lives on Grid,
/// which owns the fetches; a page lands here already render-ready.
pub(crate) struct GridDelegate {
    cols: Vec<TableColumn>,
    /// The result schema, kept so the column list can be rebuilt when the
    /// row-number preference flips.
    schema_cols: Vec<wire::Column>,
    /// Display names, one per schema column, derived once at schema commit
    /// — three surfaces (headers, popover, inspector) read them per frame.
    names: Vec<SharedString>,
    /// The gutter's absolute row numbers, derived once per page commit —
    /// render_td must not format per cell per frame.
    row_labels: Vec<SharedString>,
    /// The page's first absolute row (page × size), committed with its
    /// labels — gutter sizing derives from it when the columns rebuild.
    base: usize,
    /// Schema column indices of the primary-key columns; empty when the
    /// table has no key (and is therefore read-only).
    pk_ix: Vec<usize>,
    /// Each row's identity: the key columns' RAW fetched values, captured
    /// before display conversion — the WHERE clause binds these.
    identities: Vec<Vec<Value>>,
    /// Identity key -> row index on this page, for projecting staged
    /// changes onto the view.
    row_of: std::collections::HashMap<String, usize>,
    /// Projection of the staged layer onto this page: (row, schema col)
    /// -> staged display text (None = staged NULL).
    staged: std::collections::HashMap<(usize, usize), Option<SharedString>>,
    /// Rows staged for DELETE — ghosted with strikethrough until commit.
    deleted: std::collections::HashSet<usize>,
    /// The cell whose editor is open, and the editor to render there.
    editing: Option<(usize, usize)>,
    editor_input: Option<Entity<gpui_component::input::InputState>>,
    numeric: Vec<bool>,
    /// Schema indices hidden via the Columns popover.
    hidden: std::collections::HashSet<usize>,
    /// User drag-resizes, schema index → width, reapplied whenever the
    /// column list rebuilds (toggles, refreshes).
    widths: std::collections::HashMap<usize, Pixels>,
    /// Schema index for each data column currently displayed — the map
    /// render_td/th use, since col_ix stops matching schema order once
    /// anything is hidden.
    visible: Vec<usize>,
    /// Whether the column list currently includes the row-number gutter.
    gutter: bool,
    /// Render-ready cell text (None = NULL), converted once at page
    /// commit — render_td runs per visible cell per frame and must not
    /// allocate, so it only bumps these SharedStrings.
    rows: Vec<Vec<Option<SharedString>>>,
    /// Mirror of the table's selected row (synced from TableEvent), so
    /// render_tr can tint the selection — the delegate cannot read the
    /// TableState it is rendering inside.
    selected: Option<usize>,
    /// The Sheets corner state: "#" was clicked, every cell highlights,
    /// and the next divider double-click fits the whole table. Any
    /// ordinary cell or row click disarms it.
    all_selected: bool,
    /// The active cell (row, data column), Sheets-style: the clicked cell
    /// carries an accent ring on top of the row tint, and keyboard row
    /// moves carry the ring to the same column of the new row.
    active_cell: Option<(usize, usize)>,
    /// Set while a fetch is in flight; the TableDelegate `loading` hook
    /// reads it, so it lives here rather than on Grid.
    loading: bool,
}

impl Grid {
    /// Build a grid from an already-fetched first page. The caller fetches
    /// BEFORE constructing (DESIGN.md: fetch first, commit over the old
    /// value), so the swap from the previous grid is one complete frame —
    /// no skeleton, no columns popping in, no gutter re-widening.
    pub(crate) fn new(
        conn: Conn,
        schema: &str,
        name: &str,
        title: String,
        outcome: Result<harbor_client::QueryResult, String>,
        total_rows: Option<u64>,
        // The size the first page was FETCHED with — not re-read from
        // prefs, which may have cycled while the fetch was in flight
        // (a mismatch makes the first next-click skip rows).
        page_size: usize,
        structure: Option<crate::structure::TableStructure>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let p = prefs::get(cx);
        let gutter = p.row_numbers;
        // Editability follows capability (docs/EDITING.md): a primary key
        // from the catalog, or the grid is read-only with the reason shown.
        let pk_cols: Vec<String> = structure
            .as_ref()
            .map(|s| s.cols.iter().filter(|c| c.pk).map(|c| c.name.clone()).collect())
            .unwrap_or_default();
        let mut delegate = GridDelegate {
            cols: Vec::new(),
            schema_cols: Vec::new(),
            names: Vec::new(),
            row_labels: Vec::new(),
            base: 0,
            pk_ix: Vec::new(),
            identities: Vec::new(),
            row_of: std::collections::HashMap::new(),
            staged: std::collections::HashMap::new(),
            deleted: std::collections::HashSet::new(),
            editing: None,
            editor_input: None,
            numeric: Vec::new(),
            hidden: std::collections::HashSet::new(),
            widths: std::collections::HashMap::new(),
            visible: Vec::new(),
            gutter,
            rows: Vec::new(),
            selected: None,
            all_selected: false,
            active_cell: None,
            loading: false,
        };
        let (error, last_time_ms) = match outcome {
            Ok(page) => {
                let ms = page.time_ms;
                delegate.commit_schema(page, 0, p.zoom_factor(), &pk_cols);
                (None, ms)
            }
            Err(message) => (Some(message), 0),
        };
        let source = crate::queries::source(schema, name);
        let edits = (!delegate.pk_ix.is_empty()).then(|| {
            Edits::new(
                source.clone(),
                pk_cols.clone(),
                delegate.names.iter().map(|n| n.to_string()).collect(),
            )
        });
        let not_null = delegate
            .names
            .iter()
            .map(|n| {
                structure
                    .as_ref()
                    .and_then(|s| s.cols.iter().find(|c| c.name == n.as_ref()))
                    .is_some_and(|c| c.notnull)
            })
            .collect();
        // Header dragging stays off until move_column permutes the
        // visible map for real — the library default half-enables it
        // (widths reorder, contents don't).
        let table =
            cx.new(|cx| TableState::new(delegate, window, cx).col_movable(false));
        cx.subscribe(&table, |_, table, event: &gpui_component::table::TableEvent, cx| {
            match event {
                gpui_component::table::TableEvent::SelectRow(ix) => {
                    let ix = *ix;
                    table.update(cx, |state, cx| {
                        let d = state.delegate_mut();
                        d.selected = Some(ix);
                        // Keyboard row moves carry the active-cell ring to
                        // the same column of the new row (Sheets' arrow
                        // behavior).
                        if let Some((_, col)) = d.active_cell {
                            d.active_cell = Some((ix, col));
                        }
                        cx.notify();
                    });
                    cx.notify();
                }
                gpui_component::table::TableEvent::ColumnWidthsChanged(widths) => {
                    // Mirror drag-resizes into the delegate, keyed by
                    // schema column — otherwise any refresh rebuilds the
                    // layout from the delegate's original widths and the
                    // user's resize snaps back.
                    let widths = widths.clone();
                    table.update(cx, |state, cx| {
                        let d = state.delegate_mut();
                        let g = d.gutter as usize;
                        // With the corner's select-all armed, dragging ONE
                        // divider sizes every column to it, Sheets-style:
                        // the dragged column is the one whose width moved.
                        if d.all_selected {
                            let dragged = widths.iter().enumerate().skip(g).find_map(|(i, w)| {
                                (d.cols.get(i).map(|c| c.width) != Some(*w)).then_some(*w)
                            });
                            if let Some(w) = dragged {
                                for schema_ix in d.visible.clone() {
                                    d.widths.insert(schema_ix, w);
                                }
                                d.rebuild_cols();
                                state.refresh(cx);
                                return;
                            }
                        }
                        let d = state.delegate_mut();
                        for (i, w) in widths.iter().enumerate() {
                            if let Some(col) = d.cols.get_mut(i) {
                                col.width = *w;
                            }
                            if i >= g {
                                if let Some(&schema_ix) = d.visible.get(i - g) {
                                    d.widths.insert(schema_ix, *w);
                                }
                            }
                        }
                    });
                }
                _ => {}
            }
        })
        .detach();
        // The Table clears its selection on Escape with NO event (its
        // Cancel action calls clear_selection, which never emits), so
        // SelectRow alone lets the mirror drift: ghost tint and ring on
        // a row the table considers deselected. Reconcile on every
        // table notify instead; the comparison makes it a no-op when
        // already in sync.
        cx.observe(&table, |_, table, cx| {
            table.update(cx, |state, cx| {
                let real = state.selected_row();
                let d = state.delegate_mut();
                if d.selected != real {
                    d.selected = real;
                    if real.is_none() {
                        d.active_cell = None;
                    }
                    cx.notify();
                }
            });
        })
        .detach();
        let resize = cx.new(|_| ResizableState::default());
        cx.subscribe(&resize, |_, state, _: &ResizablePanelEvent, cx| {
            if let Some(width) = state.read(cx).sizes().get(1).copied() {
                crate::prefs::save(cx, |p| {
                    p.inspector_width = f32::from(width)
                        .clamp(prefs::INSPECTOR_MIN, prefs::INSPECTOR_MAX);
                });
            }
        })
        .detach();
        let col_search = cx.new(|cx| {
            gpui_component::input::InputState::new(window, cx)
                .placeholder("Search columns\u{2026}")
        });
        let ddl = structure.as_ref().and_then(|s| s.ddl.clone());
        let ddl_copy = ddl
            .clone()
            .map(|ddl| cx.new(|_| crate::copy_button::CopyButton::new("Copy DDL", ddl)));
        let ddl_input = ddl.map(|ddl| {
            // Auto-grow mode with the height seeded AND capped to the line
            // count (one definition per line, so the count IS the height,
            // ceiling 24 with the rest reachable by scroll). This exact
            // spelling is load-bearing: unseeded auto-grow applies its
            // measured height a frame late (the DDL flapped between two
            // rows and full size with mouse activity), and the plainer
            // `.multi_line(true).rows(n)` renders ONE row in this crate
            // version. Verified working as written; change with proof.
            let rows = ddl.lines().count().clamp(2, 24);
            cx.new(|cx| {
                gpui_component::input::InputState::new(window, cx)
                    .auto_grow(2, 24)
                    .rows(rows)
                    .default_value(ddl)
            })
        });
        cx.subscribe(&col_search, |_, _, _: &gpui_component::input::InputEvent, cx| {
            cx.notify();
        })
        .detach();
        Self {
            table,
            conn,
            source,
            title,
            page: 0,
            page_size,
            filter: None,
            total_rows,
            error,
            last_time_ms,
            edits,
            editor: None,
            pk_cols,
            not_null,
            committing: false,
            needs_focus: false,
            ring_keep: None,
            filter_input: None,
            structure,
            resize,
            col_search,
            ddl_input,
            ddl_copy,
            fetch_seq: 0,
            table_bounds: std::rc::Rc::new(std::cell::Cell::new(Bounds::default())),
        }
    }

    /// Fetch a page (and optionally a fresh count) in the background and
    /// commit everything — rows, page, size, filter — in one frame. The
    /// current page stays on screen until then; an error keeps it and
    /// shows in the strip. A newer fetch supersedes an older one in
    /// flight (the fence below), so rapid clicks converge on the latest
    /// request instead of dropping it.
    fn fetch(&mut self, req: PageReq, cx: &mut Context<Self>) {
        self.fetch_seq += 1;
        let fence = self.fetch_seq;
        let filter = match &req.filter {
            FilterChange::Set(new) => new.clone(),
            FilterChange::Keep => self.filter.clone(),
        };
        let conn = self.conn.clone();
        let sql = crate::queries::page_sql(&self.source, &filter, req.page, req.size);
        let count_sql = req.recount.then(|| crate::queries::count_sql(&self.source, &filter));
        let PageReq { page, size, filter, .. } = req;
        self.table.update(cx, |state, _| state.delegate_mut().loading = true);
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    let result = harbor_client::query(&conn, &sql)?;
                    let total = count_sql.map(|c| {
                        harbor_client::query(&conn, &c)
                            .ok()
                            .and_then(|r| crate::queries::count_of(&r))
                    });
                    Ok::<_, String>((result, total))
                })
                .await;
            this.update(cx, |grid, cx| {
                // Superseded by a newer fetch: this outcome is stale and
                // commits nothing (the newer fetch owns the loading flag).
                if grid.fetch_seq != fence {
                    return;
                }
                let result = match outcome {
                    Ok((result, total)) => {
                        grid.error = None;
                        grid.page = page;
                        grid.page_size = size;
                        if let FilterChange::Set(f) = filter {
                            grid.filter = f;
                        }
                        if let Some(t) = total {
                            grid.total_rows = t;
                        }
                        grid.last_time_ms = result.time_ms;
                        // New rows displace old indexes; an editor left
                        // open would be typing into a stranger's cell.
                        grid.editor = None;
                        Some(result)
                    }
                    Err(message) => {
                        grid.error = Some(message);
                        None
                    }
                };
                let base = page * size;
                let zoom = prefs::get(cx).zoom_factor();
                let pk_cols = grid.pk_cols.clone();
                // Taken unconditionally: a failed flip must not park a
                // stale seat for some later, unrelated fetch to restore.
                let ring_keep = grid.ring_keep.take();
                grid.table.update(cx, |state, cx| {
                    state.delegate_mut().loading = false;
                    if let Some(result) = result {
                        {
                            let d = state.delegate_mut();
                            if d.schema_cols.is_empty() {
                                // An error-born grid (first page failed)
                                // has no schema yet; adopt it from the
                                // first fetch that succeeds — the same
                                // birth Grid::new gives a healthy first
                                // page.
                                d.commit_schema(result, base, zoom, &pk_cols);
                            } else {
                                d.adopt_rows(result.rows, base);
                            }
                            d.selected = None;
                            d.active_cell = None;
                            d.editing = None;
                            d.editor_input = None;
                        }
                        let d = state.delegate();
                        if d.gutter {
                            let last = (base + d.rows.len()) as u64;
                            let want = px(gutter_width(last));
                            if d.cols[0].width != want {
                                state.delegate_mut().cols[0].width = want;
                                state.refresh(cx);
                            }
                        }
                        state.clear_selection(cx);
                        if !state.delegate().rows.is_empty() {
                            state.scroll_to_row(0, cx);
                        }
                    }
                    cx.notify();
                });
                // A keyboard page flip keeps the ring seated: same
                // column (if still visible), same row clamped to the new
                // page's rows.
                if let (Some((r, c)), true) = (ring_keep, grid.error.is_none()) {
                    grid.table.update(cx, |state, cx| {
                        let d = state.delegate_mut();
                        if d.rows.is_empty() {
                            return;
                        }
                        let row = r.min(d.rows.len() - 1);
                        let col = if d.visible.contains(&c) {
                            c
                        } else {
                            d.visible.first().copied().unwrap_or(0)
                        };
                        d.active_cell = Some((row, col));
                        select_row(state, row, cx);
                        cx.notify();
                    });
                }
                // An error-born grid earns its staging layer the moment
                // a schema lands and turns out fully keyed.
                if grid.edits.is_none() && !grid.pk_cols.is_empty() {
                    let (keyed, names) = {
                        let d = grid.table.read(cx).delegate();
                        (
                            !d.pk_ix.is_empty(),
                            d.names.iter().map(|n| n.to_string()).collect::<Vec<_>>(),
                        )
                    };
                    if keyed {
                        grid.edits = Some(Edits::new(
                            grid.source.clone(),
                            grid.pk_cols.clone(),
                            names,
                        ));
                    }
                }
                // Staged changes are identity-keyed; the new page gets
                // them projected wherever (and whether) its rows match.
                grid.sync_staged(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Navigate to a page at the current size and filter.
    fn fetch_page(&mut self, page: usize, cx: &mut Context<Self>) {
        let size = self.page_size;
        self.fetch(PageReq { page, size, filter: FilterChange::Keep, recount: false }, cx);
    }

    pub(crate) fn jump_first(&mut self, cx: &mut Context<Self>) {
        if self.page > 0 {
            self.fetch_page(0, cx);
        }
    }

    /// Jump to the last page — only reachable once the total is known,
    /// because the offset comes from it.
    pub(crate) fn jump_last(&mut self, cx: &mut Context<Self>) {
        if let Some(last) = self.last_page() {
            if self.page < last {
                self.fetch_page(last, cx);
            }
        }
    }

    pub(crate) fn prev_page(&mut self, cx: &mut Context<Self>) {
        if self.page > 0 {
            self.fetch_page(self.page - 1, cx);
        }
    }

    pub(crate) fn next_page(&mut self, cx: &mut Context<Self>) {
        if self.has_next(cx) {
            self.fetch_page(self.page + 1, cx);
        }
    }

    /// Last page index under the current count, when known.
    pub(crate) fn last_page(&self) -> Option<usize> {
        let total = self.total_rows?;
        Some((total.max(1) as usize - 1) / self.page_size)
    }

    /// Whether a next page plausibly exists. Unknown total: a full page
    /// suggests there may be more.
    pub(crate) fn has_next(&self, cx: &App) -> bool {
        match self.last_page() {
            Some(last) => self.page < last,
            None => self.table.read(cx).delegate().rows.len() == self.page_size,
        }
    }

    /// Cycle the page size through PAGE_SIZES (a global preference) and
    /// refetch from page 1. The pref cycles immediately (so rapid clicks
    /// advance through the sizes), but the delegate's size commits with
    /// the rows fetched at it — the footer never labels old rows with a
    /// new size.
    pub(crate) fn cycle_page_size(&mut self, cx: &mut Context<Self>) {
        let current = prefs::get(cx).page_size;
        let ix = prefs::PAGE_SIZES.iter().position(|s| *s == current).unwrap_or(0);
        let next = prefs::PAGE_SIZES[(ix + 1) % prefs::PAGE_SIZES.len()];
        prefs::toggle(cx, |p| p.page_size = next);
        self.fetch(PageReq { page: 0, size: next, filter: FilterChange::Keep, recount: false }, cx);
    }

    /// Open or close the raw-SQL filter strip. Closing clears an active
    /// filter (refetching unfiltered).
    pub(crate) fn toggle_filter_strip(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.filter_input.take().is_some() {
            if self.filter.is_some() {
                let size = self.page_size;
                self.fetch(
                    PageReq { page: 0, size, filter: FilterChange::Set(None), recount: true },
                    cx,
                );
            }
            cx.notify();
            return;
        }
        let input = cx.new(|cx| {
            gpui_component::input::InputState::new(window, cx)
                .placeholder("e.g. price > 100 AND name LIKE '%panel%'")
        });
        cx.subscribe(&input, |grid, input, event: &gpui_component::input::InputEvent, cx| {
            if matches!(event, gpui_component::input::InputEvent::PressEnter { .. }) {
                let text = input.read(cx).value().trim().to_string();
                let size = grid.page_size;
                grid.fetch(
                    PageReq {
                        page: 0,
                        size,
                        filter: FilterChange::Set((!text.is_empty()).then_some(text)),
                        recount: true,
                    },
                    cx,
                );
            }
        })
        .detach();
        input.update(cx, |state, cx| state.focus(window, cx));
        self.filter_input = Some(input);
        cx.notify();
    }

    /// Shared tail of every column-set mutation (returns false = no
    /// change): rebuild the display columns, refresh the table, and
    /// reset the horizontal scroll — the header (overflow_scroll) and
    /// body (virtual_list) share a scroll handle but clamp a stale
    /// offset differently once the column set changes width, so origin
    /// is the one offset they agree on.
    fn remap_columns(
        &mut self,
        cx: &mut Context<Self>,
        mutate: impl FnOnce(&mut GridDelegate) -> bool,
    ) {
        self.table.update(cx, |state, cx| {
            if !mutate(state.delegate_mut()) {
                return;
            }
            state.delegate_mut().rebuild_cols();
            state.refresh(cx);
            state.scroll_to_col(0, cx);
            cx.notify();
        });
        cx.notify();
    }

    /// Show or hide one column (never the last visible one).
    pub(crate) fn toggle_column(&mut self, schema_ix: usize, cx: &mut Context<Self>) {
        self.remap_columns(cx, |d| {
            if !d.hidden.remove(&schema_ix) {
                if d.visible.len() <= 1 {
                    return false;
                }
                d.hidden.insert(schema_ix);
            }
            true
        });
    }

    /// The Sheets divider gesture, resolved geometrically: a double-click
    /// in the header row within 4px of a column's right boundary fits that
    /// column — or, with the corner's select-all armed, every column. The
    /// boundary positions come from the delegate's own widths plus the
    /// horizontal scroll offset, so the gesture works at any scroll and on
    /// the divider line itself.
    fn divider_double_click(
        &mut self,
        e: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if e.click_count != 2 {
            return;
        }
        let bounds = self.table_bounds.get();
        let zoom = prefs::get(cx).zoom_factor();
        let header_h = prefs::get(cx).table_size().table_row_height();
        self.table.update(cx, |state, cx| {
            let y = e.position.y - bounds.origin.y;
            if y < px(0.) || y > header_h {
                return; // the gesture lives in the header row, as in Sheets
            }
            let scroll_x = state.horizontal_scroll_handle.base_handle().offset().x;
            let x = e.position.x - bounds.origin.x - scroll_x;
            let d = state.delegate_mut();
            let g = d.gutter as usize;
            let mut cum = px(0.);
            for (disp, col) in d.cols.iter().enumerate() {
                cum += col.width;
                // The gutter/first-column boundary is not a data divider.
                if disp < g {
                    continue;
                }
                if (x - cum).abs() <= px(4.) {
                    let Some(&schema_ix) = d.visible.get(disp - g) else { return };
                    if d.all_selected {
                        d.all_selected = false;
                        d.fit_widths(zoom);
                    } else {
                        d.fit_one(schema_ix, zoom);
                    }
                    state.refresh(cx);
                    return;
                }
            }
        });
    }

    /// Re-fit every column to the page on screen (View menu / Cmd-Shift-F)
    /// — the manual Sheets move, for after drags or a page whose content
    /// outgrew the first page's fit.
    pub(crate) fn fit_columns(&mut self, cx: &mut Context<Self>) {
        let zoom = prefs::get(cx).zoom_factor();
        self.table.update(cx, |state, cx| {
            state.delegate_mut().fit_widths(zoom);
            state.refresh(cx);
        });
        cx.notify();
    }

    /// Reset every hidden column (the popover's "Show all").
    pub(crate) fn show_all_columns(&mut self, cx: &mut Context<Self>) {
        self.remap_columns(cx, |d| {
            if d.hidden.is_empty() {
                return false;
            }
            d.hidden.clear();
            true
        });
    }

    /// Hide every column but the first visible one (the popover's "Hide
    /// all" — the grid never goes to zero columns, so start-from-nothing
    /// keeps one anchor to build from).
    pub(crate) fn hide_all_columns(&mut self, cx: &mut Context<Self>) {
        self.remap_columns(cx, |d| {
            if d.visible.len() <= 1 {
                return false;
            }
            let keep = d.visible[0];
            d.hidden = (0..d.schema_cols.len()).filter(|i| *i != keep).collect();
            true
        });
    }

    /// (schema index, name, hidden) for the Columns popover.
    pub(crate) fn column_list(&self, cx: &App) -> Vec<(usize, SharedString, bool)> {
        let d = self.table.read(cx).delegate();
        d.names
            .iter()
            .enumerate()
            .map(|(i, name)| (i, name.clone(), d.hidden.contains(&i)))
            .collect()
    }

    pub(crate) fn structure(&self) -> Option<&crate::structure::TableStructure> {
        self.structure.as_ref()
    }

    /// The row and (visible, gutterless) column counts, plus whether a
    /// fetch is in flight — the delegate's side of the footer's status
    /// line (footer.rs; Grid's side reads straight off the fields).
    pub(crate) fn table_facts(&self, cx: &App) -> (usize, usize, bool) {
        let d = self.table.read(cx).delegate();
        (d.rows.len(), d.cols.len().saturating_sub(d.gutter as usize), d.loading)
    }

    /// The selected row as (column, display value, is_null) pairs, for the
    /// inspector's ROW section. SharedStrings all the way — this runs on
    /// every notify with the inspector open, so it only bumps refcounts.
    /// Reads the delegate's own selection mirror, the one source render_tr
    /// tints from.
    pub(crate) fn row_kv(&self, cx: &App) -> Option<Vec<(SharedString, SharedString, bool)>> {
        let d = self.table.read(cx).delegate();
        let row = d.rows.get(d.selected?)?;
        Some(
            d.names
                .iter()
                .enumerate()
                .map(|(i, name)| match row.get(i) {
                    None | Some(None) => (name.clone(), SharedString::from("NULL"), true),
                    Some(Some(s)) => (name.clone(), s.clone(), false),
                })
                .collect(),
        )
    }

    // ------------------------------------------------------------------
    // Editing (docs/EDITING.md). One meaning per key; Esc is lossless;
    // nothing writes until ⌘S.
    // ------------------------------------------------------------------

    /// The grid's whole keymap, focus-scoped by construction: this
    /// listener sits on the grid wrapper, so it hears keys only when
    /// focus is inside — the table or an open cell editor.
    fn on_key(&mut self, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &e.keystroke;
        let m = ks.modifiers;
        if self.editor.is_some() {
            // Focus decides whose grammar a key speaks. The WHERE strip
            // and the popovers live inside this pane too; their
            // keystrokes bubble through here and are not ours.
            let (focused, replace) = self
                .editor
                .as_ref()
                .map(|ed| (ed.input.focus_handle(cx).is_focused(window), ed.replace))
                .unwrap_or((false, false));
            if !focused {
                return;
            }
            match ks.key.as_str() {
                "escape" => self.cancel_edit(cx),
                "enter" if m.platform || m.alt => return, // newline — the input's
                "enter" => {
                    self.confirm_and_move(if m.shift { -1 } else { 1 }, 0, cx);
                }
                "tab" => {
                    self.confirm_and_move(0, if m.shift { -1 } else { 1 }, cx);
                }
                "up" if replace => {
                    self.confirm_and_move(-1, 0, cx);
                }
                "down" if replace => {
                    self.confirm_and_move(1, 0, cx);
                }
                "left" if replace => {
                    self.confirm_and_move(0, -1, cx);
                }
                "right" if replace => {
                    self.confirm_and_move(0, 1, cx);
                }
                "s" if m.platform => {
                    // "I'm done, make it real": confirm in place, commit.
                    if self.confirm_and_move(0, 0, cx) {
                        self.commit(cx);
                    }
                }
                _ => return,
            }
            cx.stop_propagation();
            return;
        }
        // Navigating — but only when the table itself holds focus. A key
        // typed into the WHERE input (or any other input in the pane)
        // must mean what that input says it means.
        if !self.table.focus_handle(cx).contains_focused(window, cx) {
            return;
        }
        if m.platform && !m.shift && ks.key == "s" {
            self.commit(cx);
            cx.stop_propagation();
            return;
        }
        if m.platform && ks.key == "z" {
            let did = match &mut self.edits {
                Some(e) if m.shift => e.redo(),
                Some(e) => e.undo(),
                None => false,
            };
            if did {
                self.sync_staged(cx);
            }
            cx.stop_propagation();
            return;
        }
        if m.platform && ks.key == "backspace" {
            self.stage_delete_row(cx);
            cx.stop_propagation();
            return;
        }
        if m.control && m.shift && ks.key == "n" {
            self.stage_null(cx);
            cx.stop_propagation();
            return;
        }
        // The modified-arrow grammar (docs/EDITING.md "Navigation"), all
        // of it needing a cell to move. JUMP rides the ring's own clamp:
        // an impossible distance lands exactly on the edge.
        const JUMP: i32 = 1_000_000;
        if self.table.read(cx).delegate().active_cell.is_some() {
            // ⌘-arrows jump to the edges of the page — Sheets muscle
            // memory, scoped the way fit is: to what you're looking at.
            // (⌘⇧-arrows stay inert with the other ⇧ combos: extending a
            // selection to the edge is range territory, reserved.)
            if m.platform && !m.alt && !m.control && !m.shift {
                let (dr, dc) = match ks.key.as_str() {
                    "up" => (-JUMP, 0),
                    "down" => (JUMP, 0),
                    "left" => (0, -JUMP),
                    "right" => (0, JUMP),
                    _ => (0, 0),
                };
                if (dr, dc) != (0, 0) {
                    self.move_ring(dr, dc, cx);
                    cx.stop_propagation();
                    return;
                }
            }
            // Fn-arrows: Home/End are the column edges; the Page keys
            // drive the pager, the ring keeping its seat across the flip.
            // ⌥↑/⌥↓ alias the Page keys — reachable without Fn.
            match ks.key.as_str() {
                "home" => {
                    self.move_ring(0, -JUMP, cx);
                    cx.stop_propagation();
                    return;
                }
                "end" => {
                    self.move_ring(0, JUMP, cx);
                    cx.stop_propagation();
                    return;
                }
                "pageup" => {
                    self.page_step(-1, cx);
                    cx.stop_propagation();
                    return;
                }
                "pagedown" => {
                    self.page_step(1, cx);
                    cx.stop_propagation();
                    return;
                }
                "up" if m.alt && !m.platform && !m.control => {
                    self.page_step(-1, cx);
                    cx.stop_propagation();
                    return;
                }
                "down" if m.alt && !m.platform && !m.control => {
                    self.page_step(1, cx);
                    cx.stop_propagation();
                    return;
                }
                _ => {}
            }
        }
        if m.platform || m.control || m.function {
            return; // chords we don't own keep their meanings
        }
        let Some((row, col)) = self.table.read(cx).delegate().active_cell else {
            return;
        };
        // ⇧-arrows are deliberately inert: they are range selection's
        // seat (deferred), and a ring that moves when you expected a
        // range to grow would lie. A dead key teaches honestly.
        match ks.key.as_str() {
            "enter" => {
                self.open_editor(row, col, None, window, cx);
                cx.stop_propagation();
            }
            "backspace" | "delete" => {
                self.stage_clear(row, col, cx);
                cx.stop_propagation();
            }
            "up" if !m.shift => {
                self.move_ring(-1, 0, cx);
                cx.stop_propagation();
            }
            "down" if !m.shift => {
                self.move_ring(1, 0, cx);
                cx.stop_propagation();
            }
            "left" if !m.shift => {
                self.move_ring(0, -1, cx);
                cx.stop_propagation();
            }
            "right" if !m.shift => {
                self.move_ring(0, 1, cx);
                cx.stop_propagation();
            }
            "tab" => {
                self.move_ring(0, if m.shift { -1 } else { 1 }, cx);
                cx.stop_propagation();
            }
            "escape" => {
                // Esc while navigating clears the selection — the same
                // panic key, the same "nothing happened" result.
                self.table.update(cx, |state, cx| {
                    state.delegate_mut().active_cell = None;
                    state.clear_selection(cx);
                    cx.notify();
                });
                cx.stop_propagation();
            }
            _ => {
                // The typing contract: a printable character opens the
                // editor seeded with itself — replace entry.
                if let Some(ch) = &ks.key_char {
                    if !ch.chars().all(char::is_control) && self.edits.is_some() {
                        self.open_editor(row, col, Some(ch.clone()), window, cx);
                        cx.stop_propagation();
                    }
                }
            }
        }
    }

    /// A double-click in the table body opens the kept-value editor on
    /// the cell the first click just made active (the delegate's own
    /// mouse-down runs before this bubbling listener).
    fn on_body_click(&mut self, e: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if e.click_count != 2 || self.editor.is_some() {
            return;
        }
        // The header row is the fit gesture's turf (divider_double_click)
        // — its recorded frame excludes body double-clicks from opening
        // an editor and vice versa.
        if self.table_bounds.get().contains(&e.position) {
            return;
        }
        if let Some((row, col)) = self.table.read(cx).delegate().active_cell {
            self.open_editor(row, col, None, window, cx);
        }
    }

    /// Open the cell editor. `seed` = replace entry (the typed
    /// character); None = kept-value entry (Enter / double-click).
    fn open_editor(
        &mut self,
        row: usize,
        col: usize,
        seed: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.edits.is_none() || self.committing {
            return; // read-only says why in the footer, not with a beep
        }
        let (original, deleted) = {
            let d = self.table.read(cx).delegate();
            if row >= d.rows.len() {
                return;
            }
            let base = d.rows[row].get(col).cloned().flatten();
            let staged = d.staged.get(&(row, col)).cloned();
            (staged.unwrap_or(base), d.deleted.contains(&row))
        };
        if deleted {
            return; // you cannot edit a ghost; revert the delete first (⌘Z)
        }
        let replace = seed.is_some();
        let text = seed
            .unwrap_or_else(|| original.as_ref().map(|s| s.to_string()).unwrap_or_default());
        let input = cx.new(|cx| {
            gpui_component::input::InputState::new(window, cx).default_value(text)
        });
        input.update(cx, |state, cx| {
            // Caret at the end (set_cursor_position also focuses):
            // replace entry keeps typing past its seed; kept-value entry
            // lands where Sheets puts it. The column clamps to the line.
            state.set_cursor_position(
                gpui_component::input::Position::new(0, u32::MAX),
                window,
                cx,
            );
        });
        // Enter may be consumed by the input before it bubbles; the event
        // subscription is the belt to on_key's suspenders. Idempotent:
        // whoever runs first takes the editor.
        cx.subscribe(&input, |grid, _, ev: &gpui_component::input::InputEvent, cx| {
            if matches!(ev, gpui_component::input::InputEvent::PressEnter { .. }) {
                grid.confirm_and_move(1, 0, cx);
            }
        })
        .detach();
        self.editor = Some(CellEditor { row, col, input: input.clone(), replace });
        self.table.update(cx, |state, cx| {
            let d = state.delegate_mut();
            d.editing = Some((row, col));
            d.editor_input = Some(input);
            state.refresh(cx);
            cx.notify();
        });
        cx.notify();
    }

    /// Esc: the in-progress text never happened; what was there before —
    /// staged value or fetched value — is still there. Ring stays put.
    fn cancel_edit(&mut self, cx: &mut Context<Self>) {
        self.editor = None;
        self.close_editor_cell(cx);
    }

    fn close_editor_cell(&mut self, cx: &mut Context<Self>) {
        self.table.update(cx, |state, cx| {
            let d = state.delegate_mut();
            d.editing = None;
            d.editor_input = None;
            state.refresh(cx);
            cx.notify();
        });
        self.needs_focus = true;
        cx.notify();
    }

    /// Confirm the open editor: validate, stage (auto-clean if equal to
    /// the fetched original), close, move the ring. Returns false when
    /// validation refused — the editor stays open with the reason.
    fn confirm_and_move(&mut self, dr: i32, dc: i32, cx: &mut Context<Self>) -> bool {
        let Some(ed) = self.editor.take() else { return true };
        let text = ed.input.read(cx).value().to_string();
        let (ty, fetched, identity) = {
            let d = self.table.read(cx).delegate();
            (
                d.schema_cols.get(ed.col).map(|c| c.duckdb_type.clone()).unwrap_or_default(),
                d.rows.get(ed.row).and_then(|r| r.get(ed.col)).cloned().flatten(),
                d.identities.get(ed.row).cloned(),
            )
        };
        let staged = if text.is_empty() {
            if fetched.is_none() {
                // NULL in, nothing typed, NULL out: confirming an empty
                // editor over NULL is a no-op (stage_cell auto-cleans),
                // not a NULL→'' edit.
                Some((None, Value::Null))
            } else if edits::is_text_type(&ty) {
                // An emptied editor: '' for text (the one honest way to
                // enter it), NULL for everything else — docs/EDITING.md.
                Some((Some(SharedString::from("")), Value::String(String::new())))
            } else {
                None // NULL path, checked below
            }
        } else {
            match edits::parse_value(&text, &ty) {
                Ok(Value::Null) => None,
                Ok(v) => Some((Some(SharedString::from(text.clone())), v)),
                Err(msg) => {
                    // Validation informs, never imprisons: the editor
                    // stays open with the reason; Esc still works.
                    self.error = Some(msg);
                    self.editor = Some(ed);
                    cx.notify();
                    return false;
                }
            }
        };
        let (staged_text, value) = match staged {
            Some(pair) => pair,
            None => {
                if !self.stageable_null(ed.col, cx) {
                    self.editor = Some(ed);
                    return false;
                }
                (None, Value::Null)
            }
        };
        if let (Some(edits), Some(identity)) = (&mut self.edits, identity) {
            self.error = None;
            edits.stage_cell(identity, ed.col, fetched, staged_text, value);
        }
        self.close_editor_cell(cx);
        self.sync_staged(cx);
        if dr != 0 || dc != 0 {
            self.move_ring(dr, dc, cx);
        }
        true
    }

    /// NOT NULL columns refuse a staged NULL at the fingers, with the
    /// reason where the eyes are.
    fn stageable_null(&mut self, col: usize, cx: &mut Context<Self>) -> bool {
        if self.not_null.get(col).copied().unwrap_or(false) {
            let name = self.table.read(cx).delegate().names[col].clone();
            self.error = Some(format!("{name} is NOT NULL — edit the value instead"));
            cx.notify();
            return false;
        }
        true
    }

    /// Delete on a cell: clear it, type-honestly — '' for text columns,
    /// NULL for everything else. Never touches the row.
    fn stage_clear(&mut self, row: usize, col: usize, cx: &mut Context<Self>) {
        let (ty, fetched, identity, deleted) = {
            let d = self.table.read(cx).delegate();
            (
                d.schema_cols.get(col).map(|c| c.duckdb_type.clone()).unwrap_or_default(),
                d.rows.get(row).and_then(|r| r.get(col)).cloned().flatten(),
                d.identities.get(row).cloned(),
                d.deleted.contains(&row),
            )
        };
        if deleted {
            return;
        }
        let (text, value) = if edits::is_text_type(&ty) {
            (Some(SharedString::from("")), Value::String(String::new()))
        } else {
            if !self.stageable_null(col, cx) {
                return;
            }
            (None, Value::Null)
        };
        if let (Some(edits), Some(identity)) = (&mut self.edits, identity) {
            self.error = None;
            edits.stage_cell(identity, col, fetched, text, value);
            self.sync_staged(cx);
        }
    }

    /// ⌃⇧N: SQL NULL, deliberately, any column type.
    fn stage_null(&mut self, cx: &mut Context<Self>) {
        let Some((row, col)) = self.table.read(cx).delegate().active_cell else { return };
        let (fetched, identity) = {
            let d = self.table.read(cx).delegate();
            (
                d.rows.get(row).and_then(|r| r.get(col)).cloned().flatten(),
                d.identities.get(row).cloned(),
            )
        };
        if !self.stageable_null(col, cx) {
            return;
        }
        if let (Some(edits), Some(identity)) = (&mut self.edits, identity) {
            edits.stage_cell(identity, col, fetched, None, Value::Null);
            self.sync_staged(cx);
        }
    }

    /// ⌘⌫: stage the selected row's DELETE — visible, ghosted,
    /// reversible until commit. No dialog, ever: reversibility replaces
    /// confirmation.
    fn stage_delete_row(&mut self, cx: &mut Context<Self>) {
        let identity = {
            let d = self.table.read(cx).delegate();
            d.selected
                .or(d.active_cell.map(|(r, _)| r))
                .and_then(|r| d.identities.get(r).cloned())
        };
        if let (Some(edits), Some(identity)) = (&mut self.edits, identity) {
            edits.stage_delete(identity);
            self.sync_staged(cx);
        }
    }

    /// PageUp/PageDown (and ⌥↑/⌥↓) from the keyboard: flip the page and
    /// let the ring keep its seat — same column, same row position
    /// (clamped), new rows. A flip that cannot happen is a quiet no-op.
    fn page_step(&mut self, delta: i32, cx: &mut Context<Self>) {
        let can = if delta < 0 { self.page > 0 } else { self.has_next(cx) };
        if !can {
            return;
        }
        self.ring_keep = self.table.read(cx).delegate().active_cell;
        let page = if delta < 0 { self.page - 1 } else { self.page + 1 };
        self.fetch_page(page, cx);
    }

    /// Move the active-cell ring. Columns move along the VISIBLE order,
    /// so hidden columns don't swallow a keystroke.
    fn move_ring(&mut self, dr: i32, dc: i32, cx: &mut Context<Self>) {
        self.table.update(cx, |state, cx| {
            let d = state.delegate_mut();
            let Some((r, c)) = d.active_cell else { return };
            if d.rows.is_empty() || d.visible.is_empty() {
                return;
            }
            let nr = (r as i32 + dr).clamp(0, d.rows.len() as i32 - 1) as usize;
            let pos = d.visible.iter().position(|&v| v == c).unwrap_or(0);
            let np = (pos as i32 + dc).clamp(0, d.visible.len() as i32 - 1) as usize;
            let nc = d.visible[np];
            d.active_cell = Some((nr, nc));
            select_row(state, nr, cx);
            cx.notify();
        });
        cx.notify();
    }

    /// Project the staged layer onto the current page: identity-keyed
    /// changes land wherever (and whether) their rows appear.
    fn sync_staged(&mut self, cx: &mut Context<Self>) {
        let (staged, deleted) = {
            let d = self.table.read(cx).delegate();
            let mut staged = std::collections::HashMap::new();
            let mut deleted = std::collections::HashSet::new();
            if let Some(e) = &self.edits {
                for (key, _, change) in e.entries() {
                    let Some(&row) = d.row_of.get(key) else { continue };
                    match change {
                        edits::RowChange::Delete => {
                            deleted.insert(row);
                        }
                        edits::RowChange::Update(cells) => {
                            for (col, cell) in cells {
                                staged.insert((row, *col), cell.text.clone());
                            }
                        }
                    }
                }
            }
            (staged, deleted)
        };
        self.table.update(cx, |state, cx| {
            let d = state.delegate_mut();
            d.staged = staged;
            d.deleted = deleted;
            // Dirtiness just changed under the selection: re-decide the
            // wash (undoing a row's last edit gives its wash back).
            if let Some(row) = state.delegate().selected {
                select_row(state, row, cx);
            }
            state.refresh(cx);
            cx.notify();
        });
        cx.notify();
    }

    /// Surrender the staged layer when this grid is being replaced —
    /// only if there is actually something staged to carry.
    pub(crate) fn take_edits(&mut self) -> Option<Edits> {
        let e = self.edits.take()?;
        let (updates, deletes) = e.counts();
        if updates + deletes == 0 {
            self.edits = Some(e);
            return None;
        }
        Some(e)
    }

    /// Receive a stashed staging set from a previous visit to this
    /// table. Adopted only when the table still has the same identity
    /// and columns — a changed schema orphans the stash rather than
    /// mis-keying it.
    pub(crate) fn adopt_edits(&mut self, stash: Edits, cx: &mut Context<Self>) {
        if self.edits.as_ref().is_some_and(|mine| mine.same_shape(&stash)) {
            self.edits = Some(stash);
            self.sync_staged(cx);
        }
    }

    /// Discard one staged row change (the review popover's per-entry ✕).
    /// Itself undoable — nothing is more than one ⌘Z from recovery.
    pub(crate) fn discard_change(&mut self, key: &str, cx: &mut Context<Self>) {
        if let Some(e) = &mut self.edits {
            e.discard(key);
        }
        self.sync_staged(cx);
    }

    /// Discard everything staged — as individual discards, so each one
    /// stays on the undo stack.
    pub(crate) fn discard_all(&mut self, cx: &mut Context<Self>) {
        if let Some(e) = &mut self.edits {
            let keys: Vec<String> =
                e.entries().iter().map(|(k, _, _)| k.to_string()).collect();
            for key in keys {
                e.discard(&key);
            }
        }
        self.sync_staged(cx);
    }

    /// ⌘S: everything staged, one transaction, all or nothing. A Harbor
    /// session pins the connection so BEGIN..COMMIT outlives one request;
    /// every statement must affect exactly one row or the whole thing
    /// rolls back — and the release itself rolls back on any failure.
    pub(crate) fn commit(&mut self, cx: &mut Context<Self>) {
        if self.committing {
            return;
        }
        let Some(edits) = &self.edits else { return };
        let stmts = edits.statements();
        if stmts.is_empty() {
            return;
        }
        self.committing = true;
        self.error = None;
        cx.notify();
        let conn = self.conn.clone();
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    let sid = harbor_client::session_new(&conn)?;
                    let run = || -> Result<usize, String> {
                        harbor_client::exec(&conn, "BEGIN", None, Some(&sid))?;
                        for (sql, params) in &stmts {
                            let r = harbor_client::exec(
                                &conn,
                                sql,
                                Some(params.clone()),
                                Some(&sid),
                            )?;
                            // The engine answers UPDATE/DELETE with one
                            // count row; anything but exactly 1 means the
                            // row is not what we fetched. Nothing lands.
                            let affected = crate::queries::count_of(&r).unwrap_or(0);
                            if affected != 1 {
                                return Err(format!(
                                    "a row changed since you read it \
                                     ({affected} rows matched) — refresh and retry"
                                ));
                            }
                        }
                        harbor_client::exec(&conn, "COMMIT", None, Some(&sid))?;
                        Ok(stmts.len())
                    };
                    let result = run();
                    // Releasing the session rolls back anything uncommitted,
                    // so a failed run can never half-land.
                    harbor_client::session_release(&conn, &sid);
                    result
                })
                .await;
            this.update(cx, |grid, cx| {
                grid.committing = false;
                match outcome {
                    Ok(_) => {
                        if let Some(e) = &mut grid.edits {
                            e.clear();
                        }
                        grid.sync_staged(cx);
                        // Fetch-first: the page refetches so every row
                        // shows the database's truth — defaults filled,
                        // triggers applied.
                        let page = grid.page;
                        grid.fetch_page_now(page, cx);
                    }
                    Err(message) => {
                        grid.error = Some(format!("{message} · edits kept"));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// A refetch of the same page (post-commit) — unlike fetch_page this
    /// never skips on "already there".
    fn fetch_page_now(&mut self, page: usize, cx: &mut Context<Self>) {
        let size = self.page_size;
        self.fetch(
            PageReq { page, size, filter: FilterChange::Keep, recount: true },
            cx,
        );
    }

    /// Rebuild the column list after the row-number preference flips.
    fn sync_columns(&mut self, cx: &mut Context<Self>) {
        let want = prefs::get(cx).row_numbers;
        self.remap_columns(cx, |d| {
            if d.schema_cols.is_empty() || d.gutter == want {
                return false;
            }
            d.gutter = want;
            true
        });
    }
}

/// Select a row the dirty-aware way. The delegate always records it —
/// the ring and ⌘⌫ need a selected row — but the library's selection
/// (the blue row wash) is only requested for clean rows: once a row
/// carries staged changes, its amber or red owns the story, and two
/// washes fighting on one row read as confusion, not state.
fn select_row(
    state: &mut TableState<GridDelegate>,
    row: usize,
    cx: &mut Context<TableState<GridDelegate>>,
) {
    state.delegate_mut().selected = Some(row);
    if state.delegate().row_dirty(row) {
        state.clear_selection(cx);
        state.delegate_mut().selected = Some(row);
        state.scroll_to_row(row, cx);
    } else {
        state.set_selected_row(row, cx);
    }
}

impl GridDelegate {
    /// A row carrying any staged change — the rows whose color already
    /// tells a story, so the selection wash stays off them.
    fn row_dirty(&self, row: usize) -> bool {
        self.deleted.contains(&row) || self.staged.keys().any(|(r, _)| *r == row)
    }

    /// Adopt a page's schema and rows — the one birth, shared by Grid::new
    /// and the first successful fetch of an error-born grid. The display
    /// names are derived here, once: three surfaces (headers, popover,
    /// inspector) read them per frame and must only bump SharedStrings.
    fn commit_schema(
        &mut self,
        page: harbor_client::QueryResult,
        base: usize,
        zoom: f32,
        pk_cols: &[String],
    ) {
        self.numeric =
            page.columns.iter().map(|c| numeric(&c.duckdb_type.to_uppercase())).collect();
        self.names = page
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| SharedString::from(c.name.clone().unwrap_or_else(|| format!("col{i}"))))
            .collect();
        self.pk_ix = pk_cols
            .iter()
            .filter_map(|k| self.names.iter().position(|n| n.as_ref() == k))
            .collect();
        // Identity requires the WHOLE key: a partial match would target
        // the wrong rows, so a key column missing from the result set
        // (impossible for SELECT *, but honesty is cheap) disables it.
        if self.pk_ix.len() != pk_cols.len() {
            self.pk_ix.clear();
        }
        self.schema_cols = page.columns;
        self.adopt_rows(page.rows, base);
        // The first page sizes the columns to their content; from here on
        // widths hold still (pages replace, fits don't).
        self.fit_widths(zoom);
    }

    /// Take a page's rows: capture each row's identity (the key columns'
    /// raw values) before display conversion, then derive the render-side
    /// strings and labels. The one door rows enter the delegate through.
    fn adopt_rows(&mut self, rows: Vec<Vec<Value>>, base: usize) {
        self.identities = if self.pk_ix.is_empty() {
            Vec::new()
        } else {
            rows.iter()
                .map(|r| {
                    self.pk_ix
                        .iter()
                        .map(|&i| r.get(i).cloned().unwrap_or(Value::Null))
                        .collect()
                })
                .collect()
        };
        self.row_of = self
            .identities
            .iter()
            .enumerate()
            .map(|(ix, id)| (edits::key_of(id), ix))
            .collect();
        self.rows = display_rows(rows);
        self.relabel(base);
    }

    /// The gutter's absolute row numbers, derived once per page commit —
    /// page 2 starts at 5,001 and the label says so without a per-frame
    /// format!. `base` is the page's first absolute row (page × size),
    /// which only Grid knows.
    fn relabel(&mut self, base: usize) {
        self.base = base;
        self.row_labels = (0..self.rows.len())
            .map(|r| SharedString::from((base + r + 1).to_string()))
            .collect();
    }

    /// Content-fit every visible column from the rows in hand, Sheets
    /// style. The value font is monospace, so a column's width is its
    /// longest cell's character count times the glyph advance — no text
    /// measurement pass. Fits land in `widths`, the same slot drag-resizes
    /// use: later rebuilds keep them, page flips never re-fit, and a drag
    /// still overrides a fit.
    fn fit_widths(&mut self, zoom: f32) {
        self.rebuild_cols();
        let fits: Vec<(usize, Pixels)> = self
            .visible
            .iter()
            .map(|&ix| (ix, self.fitted_width(ix, zoom)))
            .collect();
        self.widths.extend(fits);
        self.rebuild_cols();
    }

    /// Fit a single column (double-click on its header).
    fn fit_one(&mut self, schema_ix: usize, zoom: f32) {
        if schema_ix >= self.schema_cols.len() {
            return; // the header's usize::MAX sentinel for an unmapped column
        }
        let w = self.fitted_width(schema_ix, zoom);
        self.widths.insert(schema_ix, w);
        self.rebuild_cols();
    }

    /// One column's content-fit width. Menlo's advance scales linearly
    /// (7px at 11px); the header is the proportional UI font, estimated a
    /// touch narrower. The extra covers the cell's own insets (PANE_INSET
    /// pad + 1px divider + breathing room).
    fn fitted_width(&self, schema_ix: usize, zoom: f32) -> Pixels {
        const CAP: usize = 60;
        let advance = CELL_TEXT * (7. / 11.) * zoom;
        let header_advance = HEADER_TEXT * (7. / 11.) * zoom;
        let mut chars = 4; // the NULL tag's footprint
        for row in &self.rows {
            if let Some(Some(s)) = row.get(schema_ix) {
                chars = chars.max(s.chars().count());
                if chars >= CAP {
                    break;
                }
            }
        }
        let name_len =
            self.schema_cols[schema_ix].name.as_deref().map_or(4, |n| n.chars().count());
        let content = chars.min(CAP) as f32 * advance;
        let header = name_len as f32 * header_advance;
        px((content.max(header) + PANE_INSET + 10.).clamp(60. * zoom, 460. * zoom))
    }

    /// Rebuild the display columns from the schema minus the hidden set
    /// (plus the gutter), refreshing the visible→schema map.
    fn rebuild_cols(&mut self) {
        self.visible =
            (0..self.schema_cols.len()).filter(|i| !self.hidden.contains(i)).collect();
        self.cols = build_columns(&self.names, &self.visible, self.gutter);
        let g = self.gutter as usize;
        for (disp, schema_ix) in self.visible.iter().enumerate() {
            if let Some(&w) = self.widths.get(schema_ix) {
                self.cols[disp + g].width = w;
            }
        }
        if self.gutter {
            let last = (self.base + self.rows.len()) as u64;
            self.cols[0].width = px(gutter_width(last));
        }
    }
}

impl TableDelegate for GridDelegate {
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
        // The horizontal virtual_list sizes items by MEASURING the first
        // one, so an h_full cell resolves to its content height and the
        // dividers fall short of the row lines. Cells therefore take the
        // row height explicitly and draw their own bottom border; vertical
        // and horizontal lines meet at the corners.
        let p = prefs::get(cx);
        let row_h = p.table_size().table_row_height();
        // Column 0 is the row-number gutter: raised, muted, and a firmer
        // divider than the data cells (design.css `.grid td.num`).
        if self.gutter && col_ix == 0 {
            let row_deleted = self.deleted.contains(&row_ix);
            let row_dirty = !row_deleted && self.staged.keys().any(|(r, _)| *r == row_ix);
            return div()
                .h_flex()
                .relative()
                .w_full()
                .h(row_h)
                .items_center()
                .px_1p5()
                .bg(t.raised)
                // Select-all darkens the number rail a shade deeper than
                // the cells, the way Sheets treats its row headers.
                .when(self.all_selected, |d| d.bg(t.accent.opacity(0.16)))
                // A dirty row's number wears the row's own story — amber
                // for staged updates, red for a staged delete — so dirt
                // stays findable even with its column scrolled off-screen.
                .when(row_dirty, |d| d.bg(t.warn.opacity(0.18)))
                .when(row_deleted, |d| d.bg(t.bad.opacity(0.10)))
                .border_b_1()
                .border_color(t.grid_line)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |state, _, _, cx| {
                        state.delegate_mut().all_selected = false;
                        select_row(state, row_ix, cx);
                    }),
                )
                .child(
                    div()
                        .w_full()
                        .text_right()
                        .text_size(px(GUTTER_TEXT))
                        .font_family(value_font())
                        .text_color(t.muted)
                        // Absolute position: page 2 starts at 5,001, and
                        // the number says so (plain digits, like Sheets).
                        .child(self.row_labels.get(row_ix).cloned().unwrap_or_default()),
                )
                // The gutter's divider is firmer than the data grid lines
                // (design.css `.grid td.num`), so it is its own strip.
                .child(div().absolute().right_0().top_0().bottom_0().w(px(1.)).bg(t.border))
                .into_any_element();
        }
        // Display position -> schema index, through the visible map.
        let Some(data_col) = self.visible.get(col_ix - self.gutter as usize).copied() else {
            return div().into_any_element();
        };
        // An open editor replaces the cell's content outright — the
        // editor surface IS the state (no tint underneath). It wears the
        // active-cell ring so the eye never has to relocate.
        if self.editing == Some((row_ix, data_col)) {
            if let Some(input) = self.editor_input.clone() {
                return div()
                    .h_flex()
                    .relative()
                    .w_full()
                    .h(row_h)
                    .items_center()
                    .border_r_1()
                    .border_b_1()
                    .border_color(t.grid_line)
                    .child(
                        div()
                            .absolute()
                            .left(px(-PANE_INSET))
                            .right_0()
                            .top_0()
                            .bottom_0()
                            .bg(t.surface)
                            .border_2()
                            .border_color(t.accent),
                    )
                    .child(
                        div().w_full().child(
                            gpui_component::input::Input::new(&input)
                                .appearance(false)
                                // Zero the input's built-in left inset so
                                // the caret sits exactly on the column's
                                // text axis — editing must not nudge the
                                // value sideways.
                                .pl(px(0.))
                                .text_size(px(CELL_TEXT * p.zoom_factor()))
                                .font_family(value_font()),
                        ),
                    )
                    .into_any_element();
            }
        }
        let right = p.right_align && self.numeric.get(data_col).copied().unwrap_or(false);
        // The staged layer overrides the fetched value: a confirmed edit
        // shows its new text (or NULL) under a soft accent tint until ⌘S
        // makes it the database's truth.
        let staged = self.staged.get(&(row_ix, data_col)).cloned();
        let is_staged = staged.is_some();
        let value = match staged {
            Some(v) => Some(v),
            None => self.rows.get(row_ix).and_then(|r| r.get(data_col)).cloned(),
        };
        let is_deleted = self.deleted.contains(&row_ix);
        // The column paddings are zeroed (build_columns), so this div owns
        // the cell: full height, the vertical divider on its right edge,
        // and its own text inset.
        let active = self.active_cell == Some((row_ix, data_col));
        let cell = div()
            .h_flex()
            .relative()
            .w_full()
            .h(row_h)
            .items_center()
            .pr_2()
            .border_r_1()
            .border_b_1()
            .border_color(t.grid_line)
            // The corner's select-all, made visible (Sheets: every cell
            // highlights until an ordinary click disarms it). The tint is
            // a full-bleed layer reaching back across the column wrapper's
            // PANE_INSET of left padding — on the cell box alone, the
            // padding shows through as white stripes between columns.
            .when(self.all_selected, |d| {
                d.child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(px(-PANE_INSET))
                        .right_0()
                        .bg(t.accent.opacity(0.08)),
                )
            })
            // Staged-but-uncommitted: a soft amber wash — "modified, not
            // yet saved," the color that is neither the accent (where you
            // are) nor the danger red (what you are destroying). Same
            // full-bleed layer trick as select-all, same reason.
            .when(is_staged && !is_deleted, |d| {
                d.child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(px(-PANE_INSET))
                        .right_0()
                        .bg(t.warn.opacity(0.16)),
                )
            })
            // A staged DELETE ghosts the whole row: a danger wash here,
            // strikethrough on the text below. Reversible until commit.
            .when(is_deleted, |d| {
                d.child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(px(-PANE_INSET))
                        .right_0()
                        .bg(t.bad.opacity(0.07)),
                )
            })
            // The cell starts PANE_INSET in (wrapper padding), so its
            // bottom border leaves a notch there. Every row but the LAST
            // hides it under the tr's full-width border, which the Table
            // skips on the last row; this strip patches the notch on the
            // border's own pixel (bottom -1).
            .child(
                div()
                    .absolute()
                    .left(px(-PANE_INSET))
                    .w(px(PANE_INSET))
                    .bottom(px(-1.))
                    .h(px(1.))
                    .bg(t.grid_line),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |state, _, _, cx| {
                    // Row and cell select together on mouse DOWN — the
                    // Table's own row selection waits for the click (mouse
                    // up), which reads as lag next to the ring.
                    let d = state.delegate_mut();
                    d.all_selected = false;
                    d.active_cell = Some((row_ix, data_col));
                    select_row(state, row_ix, cx);
                    cx.notify();
                }),
            )
            .when(active, |d| {
                d.child(
                    // Sheets' active-cell ring. The wrapper owns PANE_INSET
                    // of left padding, so the ring reaches back that far to
                    // sit on the cell's true grid box.
                    div()
                        .absolute()
                        .left(px(-PANE_INSET))
                        .right_0()
                        .top_0()
                        .bottom_0()
                        .border_2()
                        .border_color(t.accent),
                )
            });
        match value {
            None | Some(None) => {
                if !p.null_tags {
                    return cell.into_any_element();
                }
                cell.when(right, |d| d.justify_end())
                    .child(
                        div()
                            .flex_none()
                            .px(px(5.))
                            .rounded(px(4.))
                            .bg(t.grid_line.opacity(0.55))
                            .text_size(px(TAG_TEXT * p.zoom_factor()))
                            .font_family(ui_font())
                            .text_color(t.muted.opacity(0.65))
                            .child("NULL"),
                    )
                    .into_any_element()
            }
            Some(Some(text)) => {
                let text = text.clone();
                cell.child(
                    div()
                        .w_full()
                        .truncate()
                        .text_size(px(CELL_TEXT * p.zoom_factor()))
                        .font_family(value_font())
                        .text_color(if is_deleted { t.muted } else { t.text })
                        .when(is_deleted, |d| d.line_through())
                        .when(right, |d| d.text_right())
                        .child(text),
                )
                .into_any_element()
            }
        }
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let t = pal(cx);
        let p = prefs::get(cx);
        // The th wrapper compensates zeroed column paddings with the
        // active size's cell padding on the right (4px at XSmall, 12px at
        // Large), so a strip at right_0 lands that far inboard of the body
        // cells' dividers. right(-comp) puts it back on the true column
        // edge at every zoom level; the header row is not clipped there
        // (the clip is the padded cell box). The strip also spans the full
        // header height, where the built-in resize-handle line falls short
        // of the top and bottom.
        let comp = p.table_size().table_cell_padding().right;
        let edge = move |color: Hsla| {
            div().absolute().right(-comp).top_0().bottom_0().w(px(1.)).bg(color)
        };
        // Explicit height, like the body cells: the th sits in a chain
        // that resolves h_full to content height, so the edge strips fall
        // short of the header's top and bottom without it.
        let row_h = p.table_size().table_row_height();
        if self.gutter && col_ix == 0 {
            // Mirror the gutter's body cells (same flex centering, inset,
            // and font), so "#" sits on the numbers' baseline and shares
            // their right edge: the td inset is 6px, the wrapper already
            // padded `comp` of it, and the margin supplies the difference
            // (negative when the wrapper alone overshoots).
            return div()
                .relative()
                .h_flex()
                .items_center()
                .w_full()
                .h(row_h)
                .pl(px(6.))
                // The Sheets corner: clicking "#" highlights every cell,
                // arming the divider double-click below to fit the whole
                // table instead of one column.
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|state, _: &MouseDownEvent, _, cx| {
                        state.delegate_mut().all_selected = true;
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .w_full()
                        .text_right()
                        .mr(px(6.) - comp)
                        .text_size(px(GUTTER_TEXT))
                        .font_family(value_font())
                        .text_color(t.muted)
                        .child("#"),
                )
                .child(edge(t.border))
                .into_any_element();
        }
        let data_col =
            self.visible.get(col_ix - self.gutter as usize).copied().unwrap_or(usize::MAX);
        let right = p.right_align && self.numeric.get(data_col).copied().unwrap_or(false);
        // Left-aligned headers line up with values on the shared wrapper
        // inset by construction. A right-aligned header aims for the cell
        // text's edge, PANE_INSET + 1px divider in: the wrapper already
        // padded `comp` of it, and the margin supplies the difference
        // (negative when the wrapper alone overshoots).
        div()
            .relative()
            .h_flex()
            .items_center()
            .w_full()
            .h(row_h)
            .text_size(px(HEADER_TEXT * p.zoom_factor()))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(t.text)
            .child(
                div()
                    .w_full()
                    .truncate()
                    .when(right, |d| d.text_right().mr(px(PANE_INSET + 1.) - comp))
                    .child(self.cols[col_ix].name.clone()),
            )
            .child(edge(t.grid_line))
            .into_any_element()
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        let t = pal(cx);
        // The selection tint paints here, UNDER the cell borders, so both
        // edges of the selected row are ordinary dividers. The Table's own
        // overlay is 1px-outset (heavier top edge, missing bottom) and the
        // themes zero it out.
        div()
            .id(("row", row_ix))
            .relative()
            .when(self.selected == Some(row_ix), |d| {
                d.bg(t.row_active).child(
                    // The Table makes the selected row's own bottom border
                    // transparent (expecting its overlay border, which the
                    // themes zero). Repaint the divider on that exact
                    // pixel — bottom(-1) lands on the border-box pixel.
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom(px(-1.))
                        .h(px(1.))
                        .bg(t.grid_line),
                )
            })
    }

    fn loading(&self, _: &App) -> bool {
        self.loading && self.rows.is_empty()
    }
}

impl Render for Grid {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = pal(cx);
        let p = prefs::get(cx);
        // A closed editor hands focus back to the table here — render is
        // where a &mut Window exists (the InputEvent subscription has
        // none), so the flag set at close time is consumed one frame on.
        if self.needs_focus {
            self.needs_focus = false;
            window.focus(&self.table.focus_handle(cx));
        }
        // An editor whose column just got hidden would be invisible but
        // still focused — cancel it (lossless, like any Esc).
        if let Some(ed) = &self.editor {
            if !self.table.read(cx).delegate().visible.contains(&ed.col) {
                self.cancel_edit(cx);
            }
        }
        // The inspector slots in BESIDE the table, below the header strip —
        // the title/toggle row keeps the full width, so opening the panel
        // never shifts it. It is row-level, so it only accompanies Data.
        let inspector = (p.inspector && p.view == ViewMode::Data)
            .then(|| self.inspector(cx).into_any_element());
        let view = p.view;
        let title = self.title.clone();
        let error = self.error.clone();
        div()
            .size_full()
            .min_w_0()
            .v_flex()
            // The whole editing keymap rides the pane, on the bubble path
            // from wherever focus is — the table or an open cell editor.
            .on_key_down(cx.listener(Self::on_key))
            .child(
                div()
                    .h_flex()
                    .h_8()
                    // Left inset matches the grid text (PANE_INSET cell
                    // padding), so the title sits flush over the first
                    // column.
                    .pl(px(PANE_INSET))
                    .pr_3()
                    .gap_3()
                    .flex_none()
                    .items_center()
                    // A shade beyond raised: the column-header row below
                    // is raised, and two identical bands would merge.
                    .bg(t.strip)
                    .border_b_1()
                    .border_color(t.border)
                    .child(
                        // Semibold like the design proof's breadcrumb table
                        // name (`.crumb b`, weight 600).
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(t.text)
                            .truncate()
                            .child(title),
                    )
                    // The display toggles and inspector glyph are data
                    // concepts; the Structure view drops them.
                    .when(view == ViewMode::Data, |d| d.child(
                        // Recessed track, macOS-toolbar style: a subtle
                        // inset container; flat icon tiles with a 2px gap
                        // (edges never touch); the ON state is an
                        // accent-tinted fill. These are independent
                        // toggles, so no segment ever "wins" the track.
                        div()
                            .h_flex()
                            .flex_none()
                            .gap(px(2.))
                            .p(px(2.))
                            .rounded(px(6.))
                            // Surface track on the raised strip, the same
                            // relationship the footer seg has to its bar.
                            .bg(t.surface)
                            .border_1()
                            .border_color(t.grid_line)
                            .child(toggle_tile(
                                "toggle-rows",
                                "#",
                                "Show row numbers",
                                p.row_numbers,
                                t,
                                cx.listener(|this, _, _, cx| {
                                    prefs::toggle(cx, |p| p.row_numbers = !p.row_numbers);
                                    this.sync_columns(cx);
                                }),
                            ))
                            .child(toggle_tile(
                                "toggle-align",
                                "\u{21e5}",
                                "Right-align numeric columns",
                                p.right_align,
                                t,
                                cx.listener(|_, _, _, cx| {
                                    prefs::toggle(cx, |p| p.right_align = !p.right_align);
                                }),
                            ))
                            .child(toggle_tile(
                                "toggle-nulls",
                                "\u{2205}",
                                "Show NULL tags",
                                p.null_tags,
                                t,
                                cx.listener(|_, _, _, cx| {
                                    prefs::toggle(cx, |p| p.null_tags = !p.null_tags);
                                }),
                            )),
                    )
                    .child(
                        // The inspector's panel glyph (Finder/Xcode
                        // convention), right of the lozenge.
                        icon_tile("toggle-inspector", 22., true, t)
                            .text_color(if p.inspector { t.accent } else { t.muted })
                            .tooltip(|window, cx| {
                                Tooltip::new("Show inspector (\u{2318}\u{2325}0)").build(window, cx)
                            })
                            .on_click(cx.listener(|_, _, _, cx| {
                                prefs::toggle(cx, |p| p.inspector = !p.inspector);
                            }))
                            .child(
                                gpui_component::Icon::new(
                                    gpui_component::IconName::PanelRight,
                                )
                                .size_4(),
                            ),
                    )),
            )
            .when(view == ViewMode::Data, |d| {
                // The raw-SQL filter strip (UI.md "filters", v1): one
                // WHERE input, applied on Enter through the same
                // fetch-first swap as everything else.
                d.when_some(self.filter_input.clone(), |d, input| {
                    d.child(
                        div()
                            .h_flex()
                            .flex_none()
                            .items_center()
                            .gap_2()
                            .pl(px(PANE_INSET))
                            .pr_2()
                            .py_1()
                            .bg(t.raised)
                            .border_b_1()
                            .border_color(t.border)
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(11.))
                                    .font_family(value_font())
                                    .text_color(t.muted)
                                    .child("WHERE"),
                            )
                            .child(
                                div().flex_1().child(
                                    gpui_component::input::Input::new(&input)
                                        .xsmall()
                                        .cleanable(true),
                                ),
                            ),
                    )
                })
            })
            .when_some(error, |d, message| {
                d.child(
                    div()
                        .px_3()
                        .py_2()
                        .flex_none()
                        .text_xs()
                        .text_color(t.bad)
                        .child(message),
                )
            })
            .child(match view {
                ViewMode::Data => {
                    // The table sits in a wrapper we own, whose bounds a
                    // canvas records each frame: double-clicks anywhere in
                    // the header row hit-test against the column boundaries
                    // geometrically (widths + horizontal scroll), so the
                    // fit gesture works ON the divider line itself — the
                    // 2px the library's drag handle occludes included,
                    // because ancestors still hear what it doesn't consume.
                    let bounds_store = self.table_bounds.clone();
                    let header_h = prefs::get(cx).table_size().table_row_height();
                    let table_el = div()
                        .relative()
                        .size_full()
                        // Bubble-phase: the cell's own mouse-down (first
                        // click of the pair) has already set active_cell,
                        // so a double-click opens the editor right there.
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(Self::on_body_click),
                        )
                        .child(
                            Table::new(&self.table)
                                .bordered(false)
                                .with_size(prefs::get(cx).table_size()),
                        )
                        // Painted AFTER the table, so nothing the table
                        // occludes (drag handles, scroll containers) can
                        // eclipse it — while, carrying no occlusion of its
                        // own, everything beneath still hears its events
                        // (dragging on the line keeps working). The canvas
                        // rides INSIDE the strip: the strip's own bounds
                        // are the header-row frame the hit-test needs.
                        .child(
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .right_0()
                                .h(header_h)
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(Self::divider_double_click),
                                )
                                .child(
                                    canvas(move |b, _, _| bounds_store.set(b), |_, _, _, _| {})
                                        .size_full(),
                                ),
                        );
                    let body = div().flex_1().min_h_0().w_full();
                    match inspector {
                        // With the inspector open, the two panes share a
                        // draggable divider; the saved width seeds it.
                        Some(pane) => body.child(
                            h_resizable("data-split")
                                .with_state(&self.resize)
                                .child(
                                    resizable_panel()
                                        .child(div().size_full().child(table_el)),
                                )
                                .child(
                                    resizable_panel()
                                        .size(px(p.inspector_width))
                                        .size_range(
                                            px(prefs::INSPECTOR_MIN)..px(prefs::INSPECTOR_MAX),
                                        )
                                        .child(pane),
                                ),
                        ),
                        None => body.h_flex().child(
                            div().flex_1().min_w_0().h_full().child(table_el),
                        ),
                    }
                    .into_any_element()
                }
                ViewMode::Structure => div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(self.structure_view(cx))
                    .into_any_element(),
            })
            .child(self.footer(cx))
    }
}

/// Wire values -> render-ready cell text (None = NULL), once per page.
/// The String arm moves the buffer instead of cloning; everything else
/// pays its Display cost here so render never does.
fn display_rows(rows: Vec<Vec<Value>>) -> Vec<Vec<Option<SharedString>>> {
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|v| match v {
                    Value::Null => None,
                    Value::String(s) => Some(SharedString::from(s)),
                    other => Some(SharedString::from(other.to_string())),
                })
                .collect()
        })
        .collect()
}

fn build_columns(
    names: &[SharedString],
    visible: &[usize],
    with_gutter: bool,
) -> Vec<TableColumn> {
    // Column 0 is the row-number gutter; its render_td owns every edge.
    let gutter = with_gutter.then(|| {
        TableColumn::new("#", "#")
            .width(px(gutter_width(1)))
            .paddings(Edges::all(px(0.)))
            .resizable(false)
            .movable(false)
            .selectable(false)
    });
    gutter
        .into_iter()
        .chain(visible.iter().map(|&i| {
            // Left padding stays on the table's cell wrapper; the other
            // edges go to zero so render_td can reach them (its divider
            // and text inset live there). The width is a placeholder:
            // fit_widths sizes every column from its content before the
            // first paint.
            TableColumn::new(format!("c{i}"), names[i].clone())
                .width(px(100.))
                .paddings(Edges {
                    left: px(PANE_INSET),
                    right: px(0.),
                    top: px(0.),
                    bottom: px(0.),
                })
        }))
        .collect()
}

fn numeric(ty: &str) -> bool {
    ty.contains("INT")
        || ty.starts_with("DECIMAL")
        || ty.starts_with("NUMERIC")
        || ty.starts_with("DOUBLE")
        || ty.starts_with("FLOAT")
        || ty.starts_with("REAL")
}
