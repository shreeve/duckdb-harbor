//! The results grid: gpui-component's virtualized Table underneath (both
//! axes verified by `examples/wide_probe.rs`), DuckTable's delegate on top.
//! This surface owns fetching (server-side pages via `POST /sql`), value
//! presentation, and its header/status strips. Editing layers on later
//! (`edits.rs`); display ships first.
//!
//! Rows arrive in pages and append; a later page never clears earlier ones
//! (DESIGN.md: a refresh never clears the cache it is refreshing). Row
//! indices here are display positions in server order — the moment sorting
//! or editing arrives, reads resolve through an identity mapping, never
//! raw indices.

use crate::prefs;
use crate::theme::{pal, ui_font, value_font};
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::table::{Column as TableColumn, Table, TableDelegate, TableState};
use gpui_component::tooltip::Tooltip;
use gpui_component::resizable::{
    h_resizable, resizable_panel, ResizablePanelEvent, ResizableState,
};
use gpui_component::button::ButtonVariants as _;
use gpui_component::{Sizable as _, Size, StyledExt as _};
use harbor_client::Conn;
use serde_json::Value;

// Rows arrive as explicit pages (prefs.page_size, default 5,000): a page
// replaces the rows in one frame, so ordinary tables never hit a boundary
// and huge tables get honest jumps, constant memory, and internally
// consistent snapshots. The old infinite-append model silently stitched
// separately-queried chunks together.

// Sizes from design/design.css `.grid`: 12px mono values, 600 11.5px UI
// headers, 11px muted row numbers, 10px NULL tag.
const GRID_SIZE: Size = Size::XSmall;
const CELL_TEXT: f32 = 12.;
const HEADER_TEXT: f32 = 11.5;
const GUTTER_TEXT: f32 = 11.;
const TAG_TEXT: f32 = 10.;

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
    table: Entity<TableState<GridDelegate>>,
    /// The filter strip's input; Some = the strip is open.
    filter_input: Option<Entity<gpui_component::input::InputState>>,
    view: ViewMode,
    /// Prefetched with the first page, so switching views is instant.
    structure: Option<crate::structure::TableStructure>,
    /// The table/inspector divider (UI.md: divider positions persist —
    /// the width saves at the end of each drag).
    resize: Entity<ResizableState>,
    /// The Columns popover's search box — persistent so the query
    /// survives re-renders while the popover is open.
    col_search: Entity<gpui_component::input::InputState>,
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
    /// Schema index for each data column currently displayed — the map
    /// render_td/th use, since col_ix stops matching schema order once
    /// anything is hidden.
    visible: Vec<usize>,
    /// Whether the column list currently includes the row-number gutter.
    gutter: bool,
    /// Exact server-side row count (under the current filter), when the
    /// count query succeeded.
    pub(crate) total_rows: Option<u64>,
    /// Current page (0-based) and the size its rows were fetched with.
    page: usize,
    page_size: usize,
    /// Raw SQL WHERE clause text, verbatim from the filter strip.
    filter: Option<String>,
    rows: Vec<Vec<Value>>,
    /// Mirror of the table's selected row (synced from TableEvent), so
    /// render_tr can tint the selection — the delegate cannot read the
    /// TableState it is rendering inside.
    selected: Option<usize>,
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
            visible: Vec::new(),
            gutter,
            total_rows,
            page: 0,
            page_size: p.page_size,
            filter: None,
            rows: Vec::new(),
            selected: None,
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
                delegate.rows = page.rows;
                delegate.rebuild_cols();
            }
            Err(message) => {
                delegate.error = Some(message);
            }
        }
        let table = cx.new(|cx| TableState::new(delegate, window, cx));
        cx.subscribe(&table, |_, table, event: &gpui_component::table::TableEvent, cx| {
            if let gpui_component::table::TableEvent::SelectRow(ix) = event {
                let ix = *ix;
                table.update(cx, |state, cx| {
                    let d = state.delegate_mut();
                    d.selected = Some(ix);
                    // Keyboard row moves carry the active-cell ring to the
                    // same column of the new row (Sheets' arrow behavior).
                    if let Some((_, col)) = d.active_cell {
                        d.active_cell = Some((ix, col));
                    }
                    cx.notify();
                });
                cx.notify();
            }
        })
        .detach();
        let resize = cx.new(|_| ResizableState::default());
        cx.subscribe(&resize, |_, state, _: &ResizablePanelEvent, cx| {
            if let Some(width) = state.read(cx).sizes().get(1).copied() {
                crate::prefs::save(cx, |p| {
                    p.inspector_width = f32::from(width).clamp(180., 600.);
                });
            }
        })
        .detach();
        let col_search = cx.new(|cx| {
            gpui_component::input::InputState::new(window, cx)
                .placeholder("Search columns\u{2026}")
        });
        cx.subscribe(&col_search, |_, _, _: &gpui_component::input::InputEvent, cx| {
            cx.notify();
        })
        .detach();
        Self { table, filter_input: None, view: ViewMode::Data, structure, resize, col_search }
    }

    /// Fetch a page (and optionally a fresh count) in the background and
    /// commit everything in one frame. The current page stays on screen
    /// until then; an error keeps it and shows in the strip.
    fn fetch(&mut self, page: usize, recount: bool, cx: &mut Context<Self>) {
        let (conn, sql, count_sql) = {
            let d = self.table.read(cx).delegate();
            if d.loading {
                return;
            }
            (d.conn.clone(), d.page_sql(page, d.page_size), recount.then(|| d.count_sql()))
        };
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
                grid.table.update(cx, |state, cx| {
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
                                if d.schema_cols.is_empty() {
                                    d.numeric = result
                                        .columns
                                        .iter()
                                        .map(|c| numeric(&c.duckdb_type.to_uppercase()))
                                        .collect();
                                    d.schema_cols = result.columns;
                                    d.rebuild_cols();
                                }
                                d.rows = result.rows;
                                d.page = page;
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

    pub(crate) fn jump_first(&mut self, cx: &mut Context<Self>) {
        if self.table.read(cx).delegate().page > 0 {
            self.fetch(0, false, cx);
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
                self.fetch(last, false, cx);
            }
        }
    }

    pub(crate) fn prev_page(&mut self, cx: &mut Context<Self>) {
        let page = self.table.read(cx).delegate().page;
        if page > 0 {
            self.fetch(page - 1, false, cx);
        }
    }

    pub(crate) fn next_page(&mut self, cx: &mut Context<Self>) {
        let (page, more) = {
            let d = self.table.read(cx).delegate();
            let more = match d.last_page() {
                Some(last) => d.page < last,
                // Unknown total: a full page suggests there may be more.
                None => d.rows.len() == d.page_size,
            };
            (d.page, more)
        };
        if more {
            self.fetch(page + 1, false, cx);
        }
    }

    /// Cycle the page size through PAGE_SIZES (a global preference) and
    /// refetch from page 1.
    pub(crate) fn cycle_page_size(&mut self, cx: &mut Context<Self>) {
        let current = self.table.read(cx).delegate().page_size;
        let ix = prefs::PAGE_SIZES.iter().position(|s| *s == current).unwrap_or(0);
        let next = prefs::PAGE_SIZES[(ix + 1) % prefs::PAGE_SIZES.len()];
        prefs::toggle(cx, |p| p.page_size = next);
        self.table.update(cx, |state, _| state.delegate_mut().page_size = next);
        self.fetch(0, false, cx);
    }

    /// Open or close the raw-SQL filter strip. Closing clears an active
    /// filter (refetching unfiltered).
    pub(crate) fn toggle_filter_strip(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.filter_input.take().is_some() {
            let had_filter = self.table.read(cx).delegate().filter.is_some();
            if had_filter {
                self.table.update(cx, |state, _| state.delegate_mut().filter = None);
                self.fetch(0, true, cx);
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
                grid.table.update(cx, |state, _| {
                    state.delegate_mut().filter = (!text.is_empty()).then_some(text);
                });
                grid.fetch(0, true, cx);
            }
        })
        .detach();
        input.update(cx, |state, cx| state.focus(window, cx));
        self.filter_input = Some(input);
        cx.notify();
    }

    /// Show or hide one column (never the last visible one).
    pub(crate) fn toggle_column(&mut self, schema_ix: usize, cx: &mut Context<Self>) {
        self.table.update(cx, |state, cx| {
            {
                let d = state.delegate_mut();
                if !d.hidden.remove(&schema_ix) {
                    if d.visible.len() <= 1 {
                        return;
                    }
                    d.hidden.insert(schema_ix);
                }
                d.rebuild_cols();
            }
            state.refresh(cx);
            // Header and body clamp stale offsets differently once the
            // column set changes width; reset to origin so they agree.
            state.scroll_to_col(0, cx);
            cx.notify();
        });
        cx.notify();
    }

    /// Reset every hidden column (the popover's "Show all").
    pub(crate) fn show_all_columns(&mut self, cx: &mut Context<Self>) {
        self.table.update(cx, |state, cx| {
            {
                let d = state.delegate_mut();
                if d.hidden.is_empty() {
                    return;
                }
                d.hidden.clear();
                d.rebuild_cols();
            }
            state.refresh(cx);
            state.scroll_to_col(0, cx);
            cx.notify();
        });
        cx.notify();
    }

    /// Hide every column but the first visible one (the popover's "Hide
    /// all" — the grid never goes to zero columns, so start-from-nothing
    /// keeps one anchor to build from).
    pub(crate) fn hide_all_columns(&mut self, cx: &mut Context<Self>) {
        self.table.update(cx, |state, cx| {
            {
                let d = state.delegate_mut();
                if d.visible.len() <= 1 {
                    return;
                }
                let keep = d.visible[0];
                d.hidden = (0..d.schema_cols.len()).filter(|i| *i != keep).collect();
                d.rebuild_cols();
            }
            state.refresh(cx);
            state.scroll_to_col(0, cx);
            cx.notify();
        });
        cx.notify();
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
                        None | Some(Value::Null) => (name, "NULL".to_string(), true),
                        Some(Value::String(s)) => (name, s.clone(), false),
                        Some(v) => (name, v.to_string(), false),
                    }
                })
                .collect(),
        )
    }

    /// Rebuild the column list after the row-number preference flips.
    fn sync_columns(&mut self, cx: &mut Context<Self>) {
        let want = prefs::get(cx).row_numbers;
        self.table.update(cx, |state, cx| {
            if state.delegate().schema_cols.is_empty() || state.delegate().gutter == want {
                return;
            }
            {
                let d = state.delegate_mut();
                d.gutter = want;
                d.rebuild_cols();
            }
            state.refresh(cx);
            // The header (overflow_scroll) and body (virtual_list) share a
            // scroll handle but clamp a stale offset differently once the
            // column set changes width; reset to origin so they agree.
            state.scroll_to_col(0, cx);
            cx.notify();
        });
    }
}

impl GridDelegate {
    /// The SELECT for one page under the current filter.
    fn page_sql(&self, page: usize, size: usize) -> String {
        format!(
            "SELECT * FROM {}{} LIMIT {} OFFSET {}",
            self.source,
            self.where_part(),
            size,
            page * size,
        )
    }

    fn count_sql(&self) -> String {
        format!("SELECT count(*) FROM {}{}", self.source, self.where_part())
    }

    fn where_part(&self) -> String {
        match &self.filter {
            Some(f) => format!(" WHERE {f}"),
            None => String::new(),
        }
    }

    /// Last page index under the current count, when known.
    fn last_page(&self) -> Option<usize> {
        let total = self.total_rows?;
        Some((total.max(1) as usize - 1) / self.page_size)
    }

    /// Rebuild the display columns from the schema minus the hidden set
    /// (plus the gutter), refreshing the visible→schema map.
    fn rebuild_cols(&mut self) {
        self.visible =
            (0..self.schema_cols.len()).filter(|i| !self.hidden.contains(i)).collect();
        self.cols = build_columns(&self.schema_cols, &self.visible, self.gutter);
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
        let row_h = GRID_SIZE.table_row_height();
        let p = prefs::get(cx);
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
                    state.delegate_mut().active_cell = Some((row_ix, data_col));
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
            None | Some(Value::Null) => {
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
                            .text_size(px(TAG_TEXT))
                            .font_family(ui_font())
                            .text_color(t.muted.opacity(0.65))
                            .child("NULL"),
                    )
                    .into_any_element()
            }
            Some(v) => {
                let text = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                cell.child(
                    div()
                        .w_full()
                        .truncate()
                        .text_size(px(CELL_TEXT))
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
        // The th wrapper adds a 4px right compensation for zeroed paddings,
        // so a strip at right_0 would land 4px inboard of the body cells'
        // dividers. right(-4px) puts it on the true column edge; the header
        // row is not clipped there (the clip is the padded cell box). The
        // strip also spans the full header height, where the built-in
        // resize-handle line falls short of the top and bottom.
        let edge = |color: Hsla| {
            div().absolute().right(px(-4.)).top_0().bottom_0().w(px(1.)).bg(color)
        };
        // Explicit height, like the body cells: the th sits in a chain
        // that resolves h_full to content height, so the edge strips fall
        // short of the header's top and bottom without it.
        let row_h = GRID_SIZE.table_row_height();
        if self.gutter && col_ix == 0 {
            // Mirror the gutter's body cells (same flex centering, inset,
            // and font), so "#" sits on the numbers' baseline and shares
            // their right edge. The th wrapper adds 4px right compensation
            // the td chain doesn't have, so this cell keeps only 2px of
            // its own: 2 + 4 = the td's 6px inset.
            return div()
                .relative()
                .h_flex()
                .items_center()
                .w_full()
                .h(row_h)
                .pl(px(6.))
                .pr(px(2.))
                .child(
                    div()
                        .w_full()
                        .text_right()
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
        let right = prefs::get(cx).right_align
            && self.numeric.get(data_col).copied().unwrap_or(false);
        // Left-aligned headers line up with values on the shared 8px
        // wrapper inset by construction. A right-aligned header needs 5px
        // of its own: the wrapper compensates zeroed paddings with only
        // 4px, while cell text sits 9px in (8px pad + 1px divider).
        div()
            .relative()
            .h_flex()
            .items_center()
            .w_full()
            .h(row_h)
            .text_size(px(HEADER_TEXT))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(t.text)
            .child(
                div()
                    .w_full()
                    .truncate()
                    .when(right, |d| d.text_right().pr(px(5.)))
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = pal(cx);
        let p = prefs::get(cx);
        // The inspector slots in BESIDE the table, below the header strip —
        // the title/toggle row keeps the full width, so opening the panel
        // never shifts it. It is row-level, so it only accompanies Data.
        let inspector = (p.inspector && self.view == ViewMode::Data)
            .then(|| self.inspector(cx).into_any_element());
        let view = self.view;
        let (title, count, total, last_pg, cols, page, size, filter_active, loading, error, ms) = {
            let d = self.table.read(cx).delegate();
            (
                d.title.clone(),
                d.rows.len(),
                d.total_rows,
                d.last_page(),
                d.cols.len().saturating_sub(d.gutter as usize),
                d.page,
                d.page_size,
                d.filter.is_some(),
                d.loading,
                d.error.clone(),
                d.last_time_ms,
            )
        };
        let commas = crate::util::commas;
        let (first, last) = (page * size + 1, page * size + count);
        let rows_part = if count == 0 {
            "0 rows".to_string()
        } else {
            match total {
                Some(t) => format!(
                    "{}\u{2013}{} of {} rows",
                    commas(first as u64),
                    commas(last as u64),
                    commas(t)
                ),
                None => {
                    format!("{}\u{2013}{} rows", commas(first as u64), commas(last as u64))
                }
            }
        };
        let can_prev = page > 0;
        let can_next = match total {
            Some(t) => (last as u64) < t,
            // Unknown total: a full page suggests there may be more.
            None => count == size,
        };
        let can_last = matches!(last_pg, Some(lp) if page < lp);
        let filter_open = self.filter_input.is_some();
        // The footer status is mode-relevant, and "N columns" is ALWAYS
        // the last element: it stays fixed at the right edge when the
        // view switches, and the data-only facts simply disappear. The
        // columns text is its OWN node — as a suffix of one longer string
        // its glyphs land a subpixel differently and the switch shows a
        // 1px shift.
        //
        // Ordering rule for a jitter-free footer: in a right-justified
        // cluster an element only moves when something to its RIGHT
        // changes width, so the per-page variables (ms, row range) sit
        // leftmost and everything right of them — pager glyphs, "N per",
        // columns — is fixed. Page flips never move the arrows.
        let columns_part = format!("{cols} {}", if cols == 1 { "column" } else { "columns" });
        let loading_empty = loading && count == 0;
        let pager_visible = view == ViewMode::Data && !loading_empty;
        let status_prefix = match view {
            ViewMode::Data if loading_empty => Some("loading...".to_string()),
            ViewMode::Data => Some(format!("{ms} ms \u{00b7} {rows_part}")),
            ViewMode::Structure => None,
        };
        let status_columns =
            (view == ViewMode::Structure || pager_visible).then_some(columns_part);
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
                        div()
                            .id("toggle-inspector")
                            .h_flex()
                            .items_center()
                            .justify_center()
                            .size(px(22.))
                            .rounded(px(4.))
                            .cursor_pointer()
                            .text_color(if p.inspector { t.accent } else { t.muted })
                            .hover(|d| d.bg(t.row_hover))
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
                    let table_el =
                        Table::new(&self.table).bordered(false).with_size(GRID_SIZE);
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
                                        .size_range(px(180.)..px(600.))
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
            .child(
                // The footer (UI.md "Bottom bar", design.css `.bbar`):
                // per-table controls, a different scope from the header's
                // global display prefs. The view switcher lives here;
                // columns/filters/paging join it in later slices.
                div()
                    .h_flex()
                    .h(px(38.))
                    .flex_none()
                    .items_center()
                    .px(px(10.))
                    .bg(t.raised)
                    .border_t_1()
                    .border_color(t.border)
                    .child(
                        // design.css `.seg`, adapted: gpui's
                        // overflow_hidden does not mask child backgrounds
                        // to the rounded corners, so instead of clipped
                        // square segments the active one is an inset
                        // rounded pill (the macOS segmented-control shape).
                        // design.css `.seg`: the active fill runs flush to
                        // the track's edges. gpui does not clip child
                        // backgrounds to the track's radius, so each end
                        // segment carries its own matching outer corners
                        // (nested radius = track radius - border).
                        div()
                            .h_flex()
                            .flex_none()
                            .rounded(px(8.))
                            .bg(t.surface)
                            .border_1()
                            .border_color(t.border)
                            .child(seg_tile(
                                "view-data",
                                "Data",
                                view == ViewMode::Data,
                                (true, false),
                                t,
                                cx.listener(|this, _, _, cx| {
                                    this.view = ViewMode::Data;
                                    cx.notify();
                                }),
                            ))
                            .child(seg_tile(
                                "view-structure",
                                "Structure",
                                view == ViewMode::Structure,
                                (false, true),
                                t,
                                cx.listener(|this, _, _, cx| {
                                    this.view = ViewMode::Structure;
                                    cx.notify();
                                }),
                            )),
                    )
                    .when(view == ViewMode::Data, |d| {
                        // The filter toggle sits by the view switcher;
                        // accent when a filter is ACTIVE, not just open.
                        d.child(
                            div()
                                .id("toggle-filter")
                                .ml_2()
                                .h_flex()
                                .items_center()
                                .justify_center()
                                .size(px(22.))
                                .rounded(px(4.))
                                .cursor_pointer()
                                .hover(|d| d.bg(t.row_hover))
                                .tooltip(|window, cx| {
                                    Tooltip::new("Filter (raw SQL WHERE)").build(window, cx)
                                })
                                .child(
                                    svg()
                                        .path("icons/funnel.svg")
                                        .size_3p5()
                                        .text_color(if filter_active || filter_open {
                                            t.accent
                                        } else {
                                            t.muted
                                        }),
                                )
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.toggle_filter_strip(window, cx);
                                })),
                        )
                    })
                    .when(view == ViewMode::Data, |d| {
                        // Column show/hide, in a popover that stays open
                        // across toggles.
                        let grid = cx.entity();
                        d.child(
                            gpui_component::popover::Popover::new("columns-popover")
                                .anchor(Corner::BottomLeft)
                                .trigger(
                                    gpui_component::button::Button::new("columns-btn")
                                        .icon(gpui_component::IconName::Eye)
                                        .ghost()
                                        .xsmall()
                                        .tooltip("Show or hide columns"),
                                )
                                .content(move |_, _, cx| {
                                    let t = pal(cx);
                                    let list = grid.read(cx).column_list(cx);
                                    let total = list.len();
                                    let shown =
                                        list.iter().filter(|&&(_, _, h)| !h).count();
                                    let hidden_any = shown < total;
                                    // Same rule as the sidebar filters: a
                                    // search box only earns its row past 10
                                    // items.
                                    let searchable = total > 10;
                                    let search = grid.read(cx).col_search.clone();
                                    let query =
                                        search.read(cx).value().trim().to_lowercase();
                                    let matches: Vec<_> = list
                                        .into_iter()
                                        .filter(|(_, name, _)| {
                                            query.is_empty()
                                                || name.to_lowercase().contains(&query)
                                        })
                                        .collect();
                                    let none = matches.is_empty();
                                    // The whole row is the click target; the
                                    // Checkbox is visual only (its handler-
                                    // less listener no-ops and the click
                                    // bubbles to the row).
                                    let mut rows = div()
                                        .id("columns-list")
                                        .v_flex()
                                        .p(px(4.))
                                        .gap_px()
                                        .max_h(px(340.))
                                        .overflow_y_scroll();
                                    for (ix, name, hidden) in matches {
                                        let grid = grid.clone();
                                        rows = rows.child(
                                            div()
                                                .id(("colrow", ix))
                                                .h_flex()
                                                .items_center()
                                                .gap_2()
                                                .px(px(6.))
                                                .py(px(3.))
                                                .rounded(px(5.))
                                                .cursor_pointer()
                                                .hover(|d| d.bg(t.row_hover))
                                                .child(
                                                    gpui_component::checkbox::Checkbox::new((
                                                        "col", ix,
                                                    ))
                                                    .checked(!hidden)
                                                    .small(),
                                                )
                                                .child(
                                                    div()
                                                        .text_size(px(13.))
                                                        .map(|d| {
                                                            if hidden {
                                                                d.text_color(t.muted)
                                                            } else {
                                                                d.text_color(t.text)
                                                            }
                                                        })
                                                        .child(name),
                                                )
                                                .on_click(move |_, _, cx| {
                                                    grid.update(cx, |g, cx| {
                                                        g.toggle_column(ix, cx);
                                                    });
                                                }),
                                        );
                                    }
                                    // Header links stay put (dimmed when
                                    // inapplicable) so the row never
                                    // reflows as columns toggle.
                                    let link = |id: &'static str,
                                                label: &'static str,
                                                enabled: bool| {
                                        div().id(id).text_xs().map(|d| {
                                            if enabled {
                                                d.text_color(t.accent).cursor_pointer()
                                            } else {
                                                d.text_color(t.muted.opacity(0.5))
                                            }
                                        })
                                        .child(label)
                                    };
                                    div()
                                        .v_flex()
                                        .w(px(250.))
                                        .child(
                                            div()
                                                .h_flex()
                                                .items_center()
                                                .gap_2()
                                                .px(px(10.))
                                                .pt(px(8.))
                                                .pb(px(6.))
                                                .border_b_1()
                                                .border_color(t.border)
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .font_weight(FontWeight(560.))
                                                        .text_color(t.muted)
                                                        .child("COLUMNS"),
                                                )
                                                .when(hidden_any, |d| {
                                                    d.child(
                                                        div()
                                                            .text_xs()
                                                            .text_color(t.muted)
                                                            .child(format!(
                                                                "{shown} of {total}"
                                                            )),
                                                    )
                                                })
                                                .child(div().flex_1())
                                                .child(
                                                    link(
                                                        "cols-show-all",
                                                        "Show all",
                                                        hidden_any,
                                                    )
                                                    .when(hidden_any, |d| {
                                                        let grid = grid.clone();
                                                        d.on_click(move |_, _, cx| {
                                                            grid.update(cx, |g, cx| {
                                                                g.show_all_columns(cx);
                                                            });
                                                        })
                                                    }),
                                                )
                                                .child(
                                                    link(
                                                        "cols-hide-all",
                                                        "Hide all",
                                                        shown > 1,
                                                    )
                                                    .when(shown > 1, |d| {
                                                        let grid = grid.clone();
                                                        d.on_click(move |_, _, cx| {
                                                            grid.update(cx, |g, cx| {
                                                                g.hide_all_columns(cx);
                                                            });
                                                        })
                                                    }),
                                                ),
                                        )
                                        .when(searchable, |d| {
                                            d.child(
                                                div().px(px(8.)).pt(px(8.)).child(
                                                    gpui_component::input::Input::new(
                                                        &search,
                                                    )
                                                    .xsmall()
                                                    .cleanable(true),
                                                ),
                                            )
                                        })
                                        .when(none, |d| {
                                            d.child(
                                                div()
                                                    .px(px(10.))
                                                    .py(px(10.))
                                                    .text_xs()
                                                    .text_color(t.muted)
                                                    .child("No matching columns"),
                                            )
                                        })
                                        .child(rows)
                                        .into_any_element()
                                }),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        // One right-anchored line: ms · range · pager ·
                        // columns (see the ordering rule above).
                        div()
                            .ml_2()
                            .h_flex()
                            .flex_none()
                            .items_center()
                            .gap_2()
                            .text_xs()
                            .text_color(t.muted)
                            .when_some(status_prefix, |d, s| d.child(div().child(s)))
                            .when(pager_visible, |d| {
                                let arrow = |id: &'static str,
                                             path: &'static str,
                                             enabled: bool| {
                                    div()
                                        .id(id)
                                        .h_flex()
                                        .items_center()
                                        .justify_center()
                                        .size(px(20.))
                                        .rounded(px(4.))
                                        .map(|d| {
                                            if enabled {
                                                d.cursor_pointer()
                                                    .text_color(t.text)
                                                    .hover(|d| d.bg(t.row_hover))
                                            } else {
                                                d.text_color(t.muted.opacity(0.4))
                                            }
                                        })
                                        .child(
                                            gpui_component::Icon::empty().path(path).size_4(),
                                        )
                                };
                                d.child(div().child("\u{00b7}"))
                                    .child(
                                        div()
                                            .h_flex()
                                            .items_center()
                                            .gap_0p5()
                                            .child(
                                                arrow(
                                                    "page-first",
                                                    "icons/chevron-first.svg",
                                                    can_prev,
                                                )
                                                .on_click(cx.listener(
                                                    |this, _, _, cx| this.jump_first(cx),
                                                )),
                                            )
                                            .child(
                                                arrow(
                                                    "page-prev",
                                                    "icons/chevron-left.svg",
                                                    can_prev,
                                                )
                                                .on_click(cx.listener(
                                                    |this, _, _, cx| this.prev_page(cx),
                                                )),
                                            )
                                            .child(
                                                div()
                                                    .id("page-size")
                                                    .px_1()
                                                    .h(px(20.))
                                                    .h_flex()
                                                    .items_center()
                                                    .rounded(px(4.))
                                                    .cursor_pointer()
                                                    .hover(|d| d.bg(t.row_hover))
                                                    .tooltip(|window, cx| {
                                                        Tooltip::new(
                                                            "Rows per page \u{2014} \
                                                             click to change",
                                                        )
                                                        .build(window, cx)
                                                    })
                                                    .child(format!(
                                                        "{} per",
                                                        commas(size as u64)
                                                    ))
                                                    .on_click(cx.listener(
                                                        |this, _, _, cx| {
                                                            this.cycle_page_size(cx);
                                                        },
                                                    )),
                                            )
                                            .child(
                                                arrow(
                                                    "page-next",
                                                    "icons/chevron-right.svg",
                                                    can_next,
                                                )
                                                .on_click(cx.listener(
                                                    |this, _, _, cx| this.next_page(cx),
                                                )),
                                            )
                                            .child(
                                                arrow(
                                                    "page-last",
                                                    "icons/chevron-last.svg",
                                                    can_last,
                                                )
                                                .on_click(cx.listener(
                                                    |this, _, _, cx| this.jump_last(cx),
                                                )),
                                            ),
                                    )
                                    .child(div().child("\u{00b7}"))
                            })
                            .when_some(status_columns, |d, s| d.child(div().child(s))),
                    ),
            )
    }
}

/// One segment of the footer's view switcher (design.css `.seg span`):
/// contiguous segments on a surface track, and the active one is a SOLID
/// accent fill with on-accent text — a true segmented control, bolder
/// than the header's independent display toggles.
fn seg_tile(
    id: &'static str,
    label: &'static str,
    on: bool,
    (first, last): (bool, bool),
    t: crate::theme::Pal,
    handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let r = px(7.);
    div()
        .id(id)
        .px(px(11.))
        .py(px(3.))
        .when(first, |d| d.rounded_tl(r).rounded_bl(r))
        .when(last, |d| d.rounded_tr(r).rounded_br(r))
        .cursor_pointer()
        .text_size(px(12.))
        .map(|d| {
            if on {
                d.bg(t.accent).text_color(t.on_accent).font_weight(FontWeight(560.))
            } else {
                d.text_color(t.muted).hover(|d| d.bg(t.row_hover))
            }
        })
        .on_click(move |e, window, cx| handler(e, window, cx))
        .child(label)
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

/// Fetch a table's first page. SQL construction (quoting, paging) is this
/// module's business; app.rs calls this on a background thread before it
/// builds the grid.
pub(crate) fn first_page(
    conn: &Conn,
    schema: &str,
    name: &str,
    limit: usize,
) -> Result<harbor_client::QueryResult, String> {
    let sql = format!("SELECT * FROM {}.{} LIMIT {}", qident(schema), qident(name), limit);
    harbor_client::query(conn, &sql)
}

/// The table's exact row count, for the inspector and the status line.
pub(crate) fn total_rows(conn: &Conn, schema: &str, name: &str) -> Option<u64> {
    let sql = format!("SELECT count(*) FROM {}.{}", qident(schema), qident(name));
    let result = harbor_client::query(conn, &sql).ok()?;
    result.rows.first()?.first()?.as_u64()
}

/// Row counts for every table in one query, for the sidebar. DuckDB's
/// `estimated_size` matched exact COUNT(*) on every live table probed,
/// and the sidebar rounds to SI anyway.
pub(crate) fn table_counts(
    conn: &Conn,
) -> Option<std::collections::HashMap<(String, String), u64>> {
    let result = harbor_client::query(
        conn,
        "SELECT schema_name, table_name, estimated_size FROM duckdb_tables()",
    )
    .ok()?;
    Some(
        result
            .rows
            .iter()
            .filter_map(|row| {
                let schema = row.first()?.as_str()?.to_string();
                let table = row.get(1)?.as_str()?.to_string();
                let n = row.get(2)?.as_u64()?;
                Some(((schema, table), n))
            })
            .collect(),
    )
}

/// `PRAGMA database_size` -> (data bytes, wal bytes). The server prints
/// binary-pretty strings ("175.0 MiB"); parsing to bytes lets the app
/// render its own decimal units everywhere.
pub(crate) fn database_size(conn: &Conn) -> Option<(u64, u64)> {
    let result = harbor_client::query(conn, "PRAGMA database_size").ok()?;
    let col = |key: &str| {
        result
            .columns
            .iter()
            .position(|c| c.name.as_deref() == Some(key))
            .and_then(|i| result.rows.first()?.get(i).cloned())
            .and_then(|v| match v {
                Value::String(s) => crate::util::parse_pretty_size(&s),
                other => other.as_u64(),
            })
    };
    Some((col("database_size")?, col("wal_size")?))
}

fn qident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
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
