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

use crate::theme::{pal, value_font};
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::table::{Column as TableColumn, Table, TableDelegate, TableState};
use gpui_component::StyledExt as _;
use harbor_client::Conn;
use serde_json::Value;

const PAGE: usize = 500;

pub(crate) struct Grid {
    table: Entity<TableState<GridDelegate>>,
}

pub(crate) struct GridDelegate {
    conn: Conn,
    /// Quoted `"schema"."table"` this grid pages from.
    source: String,
    title: String,
    cols: Vec<TableColumn>,
    /// Right-aligned (numeric) columns; the Table never reads
    /// `Column::align`, so the delegate applies it to th and td itself.
    right: Vec<bool>,
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
            right: Vec::new(),
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
                                    d.right = page
                                        .columns
                                        .iter()
                                        .map(|c| numeric(&c.duckdb_type.to_uppercase()))
                                        .collect();
                                    d.cols = build_columns(&page.columns);
                                }
                                d.rows.extend(page.rows);
                            }
                            Err(message) => d.error = Some(message),
                        }
                    }
                    if first {
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
        let value = self.rows.get(row_ix).and_then(|r| r.get(col_ix));
        let (text, is_null) = match value {
            None | Some(Value::Null) => ("NULL".to_string(), true),
            Some(Value::String(s)) => (s.clone(), false),
            Some(other) => (other.to_string(), false),
        };
        // The column paddings are zeroed (build_columns), so this div owns
        // the cell: full height, the vertical divider on its right edge,
        // and its own text inset.
        div()
            .h_flex()
            .size_full()
            .items_center()
            .pr_2()
            .border_r_1()
            .border_color(t.grid_line)
            .child(
                div()
                    .w_full()
                    .truncate()
                    .font_family(value_font())
                    .text_color(if is_null { t.muted } else { t.text })
                    .when(is_null, |d| d.italic())
                    .when(
                        self.right.get(col_ix).copied().unwrap_or(false) && !is_null,
                        |d| d.text_right(),
                    )
                    .child(text),
            )
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        let t = pal(cx);
        div().id(("row", row_ix)).when(row_ix % 2 == 1, |d| d.bg(t.row_even))
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        div()
            .size_full()
            .truncate()
            .when(self.right.get(col_ix).copied().unwrap_or(false), |d| d.text_right())
            .child(self.cols[col_ix].name.clone())
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
                d.cols.len(),
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
                    .child(Table::new(&self.table).bordered(false)),
            )
    }
}

fn qident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn build_columns(cols: &[wire::Column]) -> Vec<TableColumn> {
    cols.iter()
        .enumerate()
        .map(|(i, c)| {
            let name = c.name.clone().unwrap_or_else(|| format!("col{i}"));
            let ty = c.duckdb_type.to_uppercase();
            // Left padding stays on the table's cell wrapper; the other
            // edges go to zero so render_td can reach them (its divider
            // and text inset live there).
            let col = TableColumn::new(format!("c{i}"), name)
                .width(px(width_for(&ty)))
                .paddings(Edges { left: px(8.), right: px(0.), top: px(0.), bottom: px(0.) });
            if numeric(&ty) { col.text_right() } else { col }
        })
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
