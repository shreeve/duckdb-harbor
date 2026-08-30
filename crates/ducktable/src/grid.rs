//! The results grid: gpui-component's virtualized Table underneath (both
//! axes verified by `examples/wide_probe.rs`), DuckTable's delegate on top.
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

use crate::prefs;
use crate::theme::{pal, ui_font, value_font};
use crate::util::qident;
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

// Sizes from design/design.css `.grid`: 12px mono values, 600 11.5px UI
// headers, 11px muted row numbers, 10px NULL tag.
// Base sizes at zoom 1.0; the data surfaces multiply by the current
// zoom's factor (prefs::ZOOMS), whose paired table size keeps row
// heights ahead of the text. The gutter and chrome stay put.
const CELL_TEXT: f32 = 12.;
const HEADER_TEXT: f32 = 11.5;
const GUTTER_TEXT: f32 = 11.;
const TAG_TEXT: f32 = 10.;

/// A square hover-highlight icon tile — the chassis every header/footer
/// glyph shares. Callers layer their own colors (state tints, disabled
/// dimming) on top; a disabled tile skips the pointer and hover.
pub(crate) fn icon_tile(
    id: &'static str,
    size: f32,
    enabled: bool,
    t: crate::theme::Pal,
) -> Stateful<Div> {
    div()
        .id(id)
        .h_flex()
        .items_center()
        .justify_center()
        .size(px(size))
        .rounded(px(4.))
        .when(enabled, move |d| d.cursor_pointer().hover(move |d| d.bg(t.row_hover)))
}

// 7 is Menlo's digit advance at GUTTER_TEXT (11px); 16 is the gutter's
// horizontal padding. Both move if the value font or size does.
fn gutter_width(max_row: u64) -> f32 {
    let digits = max_row.max(1).ilog10() as f32 + 1.;
    (16. + digits * 7.).max(34.)
}

/// Which view of the table the footer has selected. Data and Structure
/// are exclusive by design: a schema change reshapes the data view, so
/// the two never render side by side.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum ViewMode {
    Data,
    Structure,
}

pub(crate) struct Grid {
    // `table`, `filter_input`, and `col_search` are pub(crate) for the
    // satellite `impl Grid` files (footer.rs); nothing outside those
    // renders should touch them.
    pub(crate) table: Entity<TableState<GridDelegate>>,
    /// The filter strip's input; Some = the strip is open.
    pub(crate) filter_input: Option<Entity<gpui_component::input::InputState>>,
    view: ViewMode,
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
}

/// Everything a fetch commits along with its rows. The delegate is not
/// touched until the data arrives — the footer, funnel, and gutter always
/// describe the rows actually on screen, and a failed or superseded fetch
/// leaves no half-applied state behind.
struct PageReq {
    page: usize,
    size: usize,
    /// Some(new) = this fetch changes the WHERE filter and commits it
    /// with the rows; None = keep the current filter.
    filter: Option<Option<String>>,
    recount: bool,
}

/// The delegate facts the footer renders (footer.rs), snapshotted by
/// `Grid::footer_stats`.
pub(crate) struct FooterStats {
    pub(crate) count: usize,
    pub(crate) total: Option<u64>,
    pub(crate) page: usize,
    pub(crate) size: usize,
    pub(crate) cols: usize,
    pub(crate) can_prev: bool,
    pub(crate) can_next: bool,
    pub(crate) can_last: bool,
    pub(crate) filter_active: bool,
    pub(crate) loading: bool,
    pub(crate) ms: u64,
}

pub(crate) struct GridDelegate {
    conn: Conn,
    /// Quoted `"schema"."table"` this grid pages from.
    source: String,
    title: String,
    cols: Vec<TableColumn>,
    /// The result schema, kept so the column list can be rebuilt when the
    /// row-number preference flips.
    schema_cols: Vec<wire::Column>,
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
    /// Exact server-side row count (under the current filter), when the
    /// count query succeeded.
    total_rows: Option<u64>,
    /// Current page (0-based) and the size its rows were fetched with.
    page: usize,
    page_size: usize,
    /// Raw SQL WHERE clause text, verbatim from the filter strip.
    filter: Option<String>,
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
    loading: bool,
    error: Option<String>,
    last_time_ms: u64,
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
        let mut delegate = GridDelegate {
            conn,
            source: format!("{}.{}", qident(schema), qident(name)),
            title,
            cols: Vec::new(),
            schema_cols: Vec::new(),
            numeric: Vec::new(),
            hidden: std::collections::HashSet::new(),
            widths: std::collections::HashMap::new(),
            visible: Vec::new(),
            gutter,
            total_rows,
            page: 0,
            page_size,
            filter: None,
            rows: Vec::new(),
            selected: None,
            all_selected: false,
            active_cell: None,
            loading: false,
            error: None,
            last_time_ms: 0,
        };
        match outcome {
            Ok(page) => {
                delegate.last_time_ms = page.time_ms;
                delegate.numeric = page
                    .columns
                    .iter()
                    .map(|c| numeric(&c.duckdb_type.to_uppercase()))
                    .collect();
                delegate.schema_cols = page.columns;
                delegate.rows = display_rows(page.rows);
                // The first page sizes the columns to their content; from
                // here on widths hold still (pages replace, fits don't).
                delegate.fit_widths(p.zoom_factor());
            }
            Err(message) => {
                delegate.error = Some(message);
            }
        }
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
        let ddl_input = structure.as_ref().and_then(|s| s.ddl.clone()).map(|ddl| {
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
        let ddl_copy = structure.as_ref().and_then(|s| s.ddl.clone()).map(|ddl| {
            cx.new(|_| crate::copy_button::CopyButton::new("Copy DDL", ddl))
        });
        cx.subscribe(&col_search, |_, _, _: &gpui_component::input::InputEvent, cx| {
            cx.notify();
        })
        .detach();
        Self {
            table,
            filter_input: None,
            view: ViewMode::Data,
            structure,
            resize,
            col_search,
            ddl_input,
            ddl_copy,
            fetch_seq: 0,
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
        let (conn, sql, count_sql) = {
            let d = self.table.read(cx).delegate();
            let filter = match &req.filter {
                Some(new) => new.clone(),
                None => d.filter.clone(),
            };
            (
                d.conn.clone(),
                d.page_sql(req.page, req.size, &filter),
                req.recount.then(|| d.count_sql(&filter)),
            )
        };
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
                            .and_then(|r| r.rows.first()?.first()?.as_u64())
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
                grid.table.update(cx, |state, cx| {
                    let zoom = prefs::get(cx).zoom_factor();
                    let ok = {
                        let d = state.delegate_mut();
                        d.loading = false;
                        match outcome {
                            Ok((result, total)) => {
                                d.error = None;
                                d.last_time_ms = result.time_ms;
                                // An error-born grid (first page failed)
                                // has no schema yet; adopt it from the
                                // first fetch that succeeds, or the rows
                                // land invisible and the gutter resize
                                // below indexes an empty column list.
                                let born = d.schema_cols.is_empty();
                                if born {
                                    d.numeric = result
                                        .columns
                                        .iter()
                                        .map(|c| numeric(&c.duckdb_type.to_uppercase()))
                                        .collect();
                                    d.schema_cols = result.columns;
                                }
                                d.rows = display_rows(result.rows);
                                if born {
                                    // Its first real rows: give this grid
                                    // the same content-fit a normal birth
                                    // gets in Grid::new.
                                    d.fit_widths(zoom);
                                }
                                d.page = page;
                                d.page_size = size;
                                if let Some(f) = filter {
                                    d.filter = f;
                                }
                                if let Some(t) = total {
                                    d.total_rows = t;
                                }
                                d.selected = None;
                                d.active_cell = None;
                                true
                            }
                            Err(message) => {
                                d.error = Some(message);
                                false
                            }
                        }
                    };
                    if ok {
                        let d = state.delegate();
                        if d.gutter {
                            let last = (d.page * d.page_size + d.rows.len()) as u64;
                            let want = px(gutter_width(last));
                            if d.cols[0].width != want {
                                state.delegate_mut().cols[0].width = want;
                                state.refresh(cx);
                            }
                        }
                        state.clear_selection(cx);
                        if state.delegate().rows.len() > 0 {
                            state.scroll_to_row(0, cx);
                        }
                    }
                    cx.notify();
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Navigate to a page at the current size and filter.
    fn fetch_page(&mut self, page: usize, cx: &mut Context<Self>) {
        let size = self.table.read(cx).delegate().page_size;
        self.fetch(PageReq { page, size, filter: None, recount: false }, cx);
    }

    pub(crate) fn jump_first(&mut self, cx: &mut Context<Self>) {
        if self.table.read(cx).delegate().page > 0 {
            self.fetch_page(0, cx);
        }
    }

    /// Jump to the last page — only reachable once the total is known,
    /// because the offset comes from it.
    pub(crate) fn jump_last(&mut self, cx: &mut Context<Self>) {
        let (page, last) = {
            let d = self.table.read(cx).delegate();
            (d.page, d.last_page())
        };
        if let Some(last) = last {
            if page < last {
                self.fetch_page(last, cx);
            }
        }
    }

    pub(crate) fn prev_page(&mut self, cx: &mut Context<Self>) {
        let page = self.table.read(cx).delegate().page;
        if page > 0 {
            self.fetch_page(page - 1, cx);
        }
    }

    pub(crate) fn next_page(&mut self, cx: &mut Context<Self>) {
        let (page, more) = {
            let d = self.table.read(cx).delegate();
            (d.page, d.has_next())
        };
        if more {
            self.fetch_page(page + 1, cx);
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
        self.fetch(PageReq { page: 0, size: next, filter: None, recount: false }, cx);
    }

    /// Open or close the raw-SQL filter strip. Closing clears an active
    /// filter (refetching unfiltered).
    pub(crate) fn toggle_filter_strip(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.filter_input.take().is_some() {
            let (had_filter, size) = {
                let d = self.table.read(cx).delegate();
                (d.filter.is_some(), d.page_size)
            };
            if had_filter {
                self.fetch(
                    PageReq { page: 0, size, filter: Some(None), recount: true },
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
                let size = grid.table.read(cx).delegate().page_size;
                grid.fetch(
                    PageReq {
                        page: 0,
                        size,
                        filter: Some((!text.is_empty()).then_some(text)),
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

    /// Reset every hidden column (the popover's "Show all").
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
    pub(crate) fn column_list(&self, cx: &App) -> Vec<(usize, String, bool)> {
        let d = self.table.read(cx).delegate();
        d.schema_cols
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let name = c.name.clone().unwrap_or_else(|| format!("col{i}"));
                (i, name, d.hidden.contains(&i))
            })
            .collect()
    }

    pub(crate) fn structure(&self) -> Option<&crate::structure::TableStructure> {
        self.structure.as_ref()
    }

    /// The footer's view choice survives a table switch: the new grid is
    /// seeded with the outgoing grid's mode (app.rs select_table).
    pub(crate) fn view(&self) -> ViewMode {
        self.view
    }

    pub(crate) fn set_view(&mut self, view: ViewMode) {
        self.view = view;
    }

    /// Everything the footer's status line and pager need, snapshotted
    /// once per render (footer.rs cannot read the delegate's fields).
    pub(crate) fn footer_stats(&self, cx: &App) -> FooterStats {
        let d = self.table.read(cx).delegate();
        FooterStats {
            count: d.rows.len(),
            total: d.total_rows,
            page: d.page,
            size: d.page_size,
            cols: d.cols.len().saturating_sub(d.gutter as usize),
            can_prev: d.page > 0,
            can_next: d.has_next(),
            can_last: matches!(d.last_page(), Some(lp) if d.page < lp),
            filter_active: d.filter.is_some(),
            loading: d.loading,
            ms: d.last_time_ms,
        }
    }

    /// The selected row as (column, display value, is_null) pairs, for the
    /// inspector's ROW section.
    pub(crate) fn row_kv(&self, cx: &App) -> Option<Vec<(String, String, bool)>> {
        let state = self.table.read(cx);
        let d = state.delegate();
        let row = d.rows.get(state.selected_row()?)?;
        Some(
            d.schema_cols
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let name = c.name.clone().unwrap_or_else(|| format!("col{i}"));
                    match row.get(i) {
                        None | Some(None) => (name, "NULL".to_string(), true),
                        Some(Some(s)) => (name, s.to_string(), false),
                    }
                })
                .collect(),
        )
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

impl GridDelegate {
    /// The SELECT for one page under the current filter.
    fn page_sql(&self, page: usize, size: usize, filter: &Option<String>) -> String {
        format!(
            "SELECT * FROM {}{} LIMIT {} OFFSET {}",
            self.source,
            Self::where_part(filter),
            size,
            page * size,
        )
    }

    fn count_sql(&self, filter: &Option<String>) -> String {
        format!("SELECT count(*) FROM {}{}", self.source, Self::where_part(filter))
    }

    // The filter text splices in verbatim BY DESIGN: the strip is a raw
    // SQL surface and the berth is the user's own database — the author
    // of the WHERE clause is the person it could affect.
    fn where_part(filter: &Option<String>) -> String {
        match filter {
            Some(f) => format!(" WHERE {f}"),
            None => String::new(),
        }
    }

    /// Last page index under the current count, when known.
    fn last_page(&self) -> Option<usize> {
        let total = self.total_rows?;
        Some((total.max(1) as usize - 1) / self.page_size)
    }

    /// Whether a next page plausibly exists. Unknown total: a full page
    /// suggests there may be more.
    fn has_next(&self) -> bool {
        match self.last_page() {
            Some(last) => self.page < last,
            None => self.rows.len() == self.page_size,
        }
    }

    /// Rebuild the display columns from the schema minus the hidden set
    /// (plus the gutter), refreshing the visible→schema map.
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
    /// touch narrower. 18 covers the cell's own insets (8px pad + 1px
    /// divider + breathing room).
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
        px((content.max(header) + 18.).clamp(60. * zoom, 460. * zoom))
    }

    fn rebuild_cols(&mut self) {
        self.visible =
            (0..self.schema_cols.len()).filter(|i| !self.hidden.contains(i)).collect();
        self.cols = build_columns(&self.schema_cols, &self.visible, self.gutter);
        let g = self.gutter as usize;
        for (disp, schema_ix) in self.visible.iter().enumerate() {
            if let Some(&w) = self.widths.get(schema_ix) {
                self.cols[disp + g].width = w;
            }
        }
        if self.gutter {
            let last = (self.page * self.page_size + self.rows.len()) as u64;
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
            return div()
                .h_flex()
                .relative()
                .w_full()
                .h(row_h)
                .items_center()
                .px_1p5()
                .bg(t.raised)
                .border_b_1()
                .border_color(t.grid_line)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |state, _, _, cx| {
                        state.delegate_mut().all_selected = false;
                        state.set_selected_row(row_ix, cx);
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
                        .child(format!("{}", self.page * self.page_size + row_ix + 1)),
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
        let right = p.right_align && self.numeric.get(data_col).copied().unwrap_or(false);
        let value = self.rows.get(row_ix).and_then(|r| r.get(data_col));
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
            // highlights until an ordinary click disarms it).
            .when(self.all_selected, |d| d.bg(t.accent.opacity(0.08)))
            // The cell starts 8px in (wrapper padding), so its bottom
            // border leaves a notch there. Every row but the LAST hides
            // it under the tr's full-width border, which the Table skips
            // on the last row; this strip patches the notch on the
            // border's own pixel (bottom -1).
            .child(
                div()
                    .absolute()
                    .left(px(-8.))
                    .w(px(8.))
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
                    state.set_selected_row(row_ix, cx);
                    cx.notify();
                }),
            )
            .when(active, |d| {
                d.child(
                    // Sheets' active-cell ring. The wrapper owns 8px of
                    // left padding, so the ring reaches left(-8) to sit
                    // on the cell's true grid box.
                    div()
                        .absolute()
                        .left(px(-8.))
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
                        .text_color(t.text)
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
        // text's edge, 9px in (8px pad + 1px divider): the wrapper already
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
                    .when(right, |d| d.text_right().mr(px(9.) - comp))
                    .child(self.cols[col_ix].name.clone()),
            )
            .child(edge(t.grid_line))
            // The Sheets fit gesture, on the line it belongs to: double-
            // click a column's right divider to fit that column — or, with
            // the corner's select-all armed, to fit every column. The strip
            // hugs the line from the left; the library's 2px drag handle
            // occludes only the line itself, so this stays clickable and
            // sits inside the resize cursor's approach zone.
            .child(
                div()
                    .id(("fit-divider", col_ix))
                    .absolute()
                    .right(-comp)
                    .top_0()
                    .bottom_0()
                    .w(px(8.))
                    .cursor_col_resize()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |state, e: &MouseDownEvent, _, cx| {
                            if e.click_count == 2 {
                                let zoom = prefs::get(cx).zoom_factor();
                                let d = state.delegate_mut();
                                if d.all_selected {
                                    d.all_selected = false;
                                    d.fit_widths(zoom);
                                } else {
                                    d.fit_one(data_col, zoom);
                                }
                                state.refresh(cx);
                            }
                        }),
                    ),
            )
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = pal(cx);
        let p = prefs::get(cx);
        // The inspector slots in BESIDE the table, below the header strip —
        // the title/toggle row keeps the full width, so opening the panel
        // never shifts it. It is row-level, so it only accompanies Data.
        let inspector = (p.inspector && self.view == ViewMode::Data)
            .then(|| self.inspector(cx).into_any_element());
        let view = self.view;
        let (title, error) = {
            let d = self.table.read(cx).delegate();
            (d.title.clone(), d.error.clone())
        };
        div()
            .size_full()
            .min_w_0()
            .v_flex()
            .child(
                div()
                    .h_flex()
                    .h_8()
                    // Left inset matches the grid text (8px cell padding),
                    // so the title sits flush over the first column.
                    .pl_2()
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
                            .px_2()
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
                    let table_el = Table::new(&self.table)
                        .bordered(false)
                        .with_size(prefs::get(cx).table_size());
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

/// One tile in the display-toggle track: flat glyph when off, accent-tinted
/// fill when on, faint hover, tooltip on hover.
fn toggle_tile(
    id: &'static str,
    glyph: &'static str,
    tip: &'static str,
    on: bool,
    t: crate::theme::Pal,
    handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    div()
        .id(id)
        .h_flex()
        .items_center()
        .justify_center()
        .w(px(24.))
        .h(px(18.))
        .rounded(px(4.))
        .cursor_pointer()
        .text_size(px(11.))
        .map(|d| {
            if on {
                d.bg(t.accent.opacity(0.15)).text_color(t.accent)
            } else {
                d.text_color(t.muted)
            }
        })
        .hover(move |d| if on { d } else { d.bg(t.row_hover) })
        .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
        .on_click(move |e, window, cx| handler(e, window, cx))
        .child(glyph)
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
    cols: &[wire::Column],
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
            let c = &cols[i];
            let name = c.name.clone().unwrap_or_else(|| format!("col{i}"));
            let ty = c.duckdb_type.to_uppercase();
            // Left padding stays on the table's cell wrapper; the other
            // edges go to zero so render_td can reach them (its divider
            // and text inset live there).
            TableColumn::new(format!("c{i}"), name)
                .width(px(width_for(&ty)))
                .paddings(Edges { left: px(8.), right: px(0.), top: px(0.), bottom: px(0.) })
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

fn width_for(ty: &str) -> f32 {
    if ty == "BOOLEAN" {
        80.
    } else if ty == "UUID" {
        290.
    } else if ty.starts_with("TIMESTAMP") {
        190.
    } else if ty == "DATE" || ty.starts_with("TIME") {
        110.
    } else if numeric(ty) {
        100.
    } else {
        200.
    }
}
