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

use crate::theme::{pal, ui_font, value_font};
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::table::{Column as TableColumn, Table, TableDelegate, TableState};
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
    rows: Vec<Vec<Value>>,
    eof: bool,
    loading: bool,
    error: Option<String>,
    last_time_ms: u64,
}

impl Grid {
    pub(crate) fn new(
        conn: Conn,
        schema: &str,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = GridDelegate {
            conn,
            source: format!("{}.{}", qident(schema), qident(name)),
            title: format!("{schema}.{name}"),
            cols: Vec::new(),
            rows: Vec::new(),
            eof: false,
            loading: false,
            error: None,
            last_time_ms: 0,
        };
        let table = cx.new(|cx| {
            let mut state = TableState::new(delegate, window, cx);
            state.delegate_mut().fetch_next_page(cx);
            state
        });
        Self { table }
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
                    let first = state.delegate().cols.is_empty();
                    {
                        let d = state.delegate_mut();
                        d.loading = false;
                        match outcome {
                            Ok(page) => {
                                d.eof = page.rows.len() < PAGE;
                                d.last_time_ms = page.time_ms;
                                if first {
                                    d.cols = build_columns(&page.columns);
                                }
                                d.rows.extend(page.rows);
                            }
                            Err(message) => d.error = Some(message),
                        }
                    }
                    // The row-number gutter widens as pages land; a width
                    // change needs the col groups re-prepared.
                    let want = px(gutter_width(state.delegate().rows.len()));
                    let widen = state
                        .delegate()
                        .cols
                        .first()
                        .is_some_and(|c| c.width != want);
                    if widen {
                        state.delegate_mut().cols[0].width = want;
                    }
                    if first || widen {
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
        // Column 0 is the row-number gutter: raised, muted, and a firmer
        // divider than the data cells (design.css `.grid td.num`).
        if col_ix == 0 {
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
        let data_col = col_ix - 1;
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
            None | Some(Value::Null) => cell
                .child(
                    div()
                        .flex_none()
                        .px(px(5.))
                        .rounded(px(4.))
                        .bg(t.grid_line)
                        .text_size(px(TAG_TEXT))
                        .font_family(ui_font())
                        .text_color(t.muted)
                        .child("NULL"),
                )
                .into_any_element(),
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
        if col_ix == 0 {
            return div()
                .size_full()
                .text_right()
                .text_size(px(GUTTER_TEXT))
                .text_color(t.muted)
                .child("#")
                .into_any_element();
        }
        // Headers and values are both left-aligned (design proof: only the
        // row-number gutter right-aligns), so they line up on the shared
        // 8px wrapper inset by construction.
        div()
            .size_full()
            .truncate()
            .text_size(px(HEADER_TEXT))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(t.text)
            .child(self.cols[col_ix].name.clone())
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
        let (title, count, cols, eof, loading, error, ms) = {
            let d = self.table.read(cx).delegate();
            (
                d.title.clone(),
                d.rows.len(),
                d.cols.len().saturating_sub(1),
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
                    .px_3()
                    .flex_none()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(t.border)
                    .child(div().text_sm().text_color(t.text).truncate().child(title))
                    .child(div().text_xs().text_color(t.muted).flex_none().child(status)),
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

fn qident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn build_columns(cols: &[wire::Column]) -> Vec<TableColumn> {
    // Column 0 is the row-number gutter; its render_td owns every edge.
    let gutter = TableColumn::new("#", "#")
        .width(px(gutter_width(1)))
        .paddings(Edges::all(px(0.)))
        .resizable(false)
        .movable(false)
        .selectable(false);
    std::iter::once(gutter)
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
