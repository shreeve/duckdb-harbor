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
use gpui_component::{Sizable as _, Size, StyledExt as _};
use harbor_client::Conn;
use serde_json::Value;

const PAGE: usize = 500;

// Sizes from design/design.css `.grid`: 12px mono values, 600 11.5px UI
// headers, 11px muted row numbers, 10px NULL tag.
const GRID_SIZE: Size = Size::XSmall;
const CELL_TEXT: f32 = 12.;
const HEADER_TEXT: f32 = 11.5;
const GUTTER_TEXT: f32 = 11.;
const TAG_TEXT: f32 = 10.;

fn gutter_width(rows: usize) -> f32 {
    let digits = rows.max(1).ilog10() as f32 + 1.;
    (16. + digits * 7.).max(34.)
}

pub(crate) struct Grid {
    table: Entity<TableState<GridDelegate>>,
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
    /// Whether the column list currently includes the row-number gutter.
    gutter: bool,
    rows: Vec<Vec<Value>>,
    eof: bool,
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let gutter = prefs::get(cx).row_numbers;
        let mut delegate = GridDelegate {
            conn,
            source: format!("{}.{}", qident(schema), qident(name)),
            title,
            cols: Vec::new(),
            schema_cols: Vec::new(),
            numeric: Vec::new(),
            gutter,
            rows: Vec::new(),
            eof: false,
            loading: false,
            error: None,
            last_time_ms: 0,
        };
        match outcome {
            Ok(page) => {
                delegate.eof = page.rows.len() < PAGE;
                delegate.last_time_ms = page.time_ms;
                delegate.numeric = page
                    .columns
                    .iter()
                    .map(|c| numeric(&c.duckdb_type.to_uppercase()))
                    .collect();
                delegate.cols = build_columns(&page.columns, gutter);
                if gutter {
                    delegate.cols[0].width = px(gutter_width(page.rows.len()));
                }
                delegate.schema_cols = page.columns;
                delegate.rows = page.rows;
            }
            Err(message) => {
                delegate.error = Some(message);
                delegate.eof = true;
            }
        }
        let table = cx.new(|cx| TableState::new(delegate, window, cx));
        Self { table }
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
                d.cols = build_columns(&d.schema_cols, want);
                if want {
                    d.cols[0].width = px(gutter_width(d.rows.len()));
                }
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
    fn fetch_next_page(&mut self, cx: &mut Context<TableState<Self>>) {
        if self.eof || self.loading || self.error.is_some() {
            return;
        }
        self.loading = true;
        let conn = self.conn.clone();
        let sql =
            format!("SELECT * FROM {} LIMIT {} OFFSET {}", self.source, PAGE, self.rows.len());
        cx.spawn(async move |state, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move { harbor_client::query(&conn, &sql) })
                .await;
            state
                .update(cx, |state, cx| {
                    {
                        let d = state.delegate_mut();
                        d.loading = false;
                        match outcome {
                            Ok(page) => {
                                d.eof = page.rows.len() < PAGE;
                                d.last_time_ms = page.time_ms;
                                d.rows.extend(page.rows);
                            }
                            Err(message) => d.error = Some(message),
                        }
                    }
                    // The row-number gutter widens as pages land; a width
                    // change needs the col groups re-prepared.
                    let want = px(gutter_width(state.delegate().rows.len()));
                    let widen = state.delegate().gutter
                        && state
                            .delegate()
                            .cols
                            .first()
                            .is_some_and(|c| c.width != want);
                    if widen {
                        state.delegate_mut().cols[0].width = want;
                        state.refresh(cx);
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
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
                .child(
                    div()
                        .w_full()
                        .text_right()
                        .text_size(px(GUTTER_TEXT))
                        .font_family(value_font())
                        .text_color(t.muted)
                        .child(format!("{}", row_ix + 1)),
                )
                // The gutter's divider is firmer than the data grid lines
                // (design.css `.grid td.num`), so it is its own strip.
                .child(div().absolute().right_0().top_0().bottom_0().w(px(1.)).bg(t.border))
                .into_any_element();
        }
        let data_col = col_ix - self.gutter as usize;
        let right = p.right_align && self.numeric.get(data_col).copied().unwrap_or(false);
        let value = self.rows.get(row_ix).and_then(|r| r.get(data_col));
        // The column paddings are zeroed (build_columns), so this div owns
        // the cell: full height, the vertical divider on its right edge,
        // and its own text inset.
        let cell = div()
            .h_flex()
            .w_full()
            .h(row_h)
            .items_center()
            .pr_2()
            .border_r_1()
            .border_b_1()
            .border_color(t.grid_line);
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
        let data_col = col_ix - self.gutter as usize;
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

    fn is_eof(&self, _: &App) -> bool {
        self.eof
    }

    fn load_more(&mut self, _: &mut Window, cx: &mut Context<TableState<Self>>) {
        self.fetch_next_page(cx);
    }

    fn loading(&self, _: &App) -> bool {
        self.loading && self.rows.is_empty()
    }
}

impl Render for Grid {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = pal(cx);
        let p = prefs::get(cx);
        let (title, count, cols, eof, loading, error, ms) = {
            let d = self.table.read(cx).delegate();
            (
                d.title.clone(),
                d.rows.len(),
                d.cols.len().saturating_sub(d.gutter as usize),
                d.eof,
                d.loading,
                d.error.clone(),
                d.last_time_ms,
            )
        };
        let status = if loading && count == 0 {
            "loading...".to_string()
        } else {
            format!(
                "{count}{} {} \u{00b7} {cols} {} \u{00b7} {ms} ms",
                if eof { "" } else { "+" },
                if count == 1 && eof { "row" } else { "rows" },
                if cols == 1 { "column" } else { "columns" },
            )
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
                    // Status text sits INSIDE the flexible region, before
                    // the toggle track: its width changes as pages land,
                    // and text may move but controls must not.
                    .child(div().text_xs().text_color(t.muted).flex_none().child(status))
                    .child(
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
                            .bg(t.raised)
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
                    ),
            )
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
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(Table::new(&self.table).bordered(false).with_size(GRID_SIZE)),
            )
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

/// Fetch a table's first page. SQL construction (quoting, paging) is this
/// module's business; app.rs calls this on a background thread before it
/// builds the grid.
pub(crate) fn first_page(
    conn: &Conn,
    schema: &str,
    name: &str,
) -> Result<harbor_client::QueryResult, String> {
    let sql = format!("SELECT * FROM {}.{} LIMIT {}", qident(schema), qident(name), PAGE);
    harbor_client::query(conn, &sql)
}

fn qident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn build_columns(cols: &[wire::Column], with_gutter: bool) -> Vec<TableColumn> {
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
        .chain(cols.iter().enumerate().map(|(i, c)| {
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
