//! The Structure view: the table's columns and DDL (UI.md "Bottom bar"),
//! swapped in for the data grid via the footer's view switcher. Viewing
//! data and viewing schema are exclusive by design — a schema change
//! reshapes the data view, so the two never render side by side. Read-only
//! in this slice; the editor arrives with the staged/live pipeline
//! (edits.rs).
//!
//! Introspection contract verified against a live berth
//! (crates/harbor-client/examples/probe_struct.rs): `PRAGMA
//! table_info('"schema"."table"')` returns cid, name, type,
//! notnull (bool), dflt_value, pk (bool); `duckdb_tables()` carries the
//! reconstructed CREATE TABLE in its `sql` column.

use crate::grid::Grid;
use crate::theme::{pal, ui_font, value_font, Pal};
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::StyledExt as _;
use harbor_client::Conn;
use serde_json::Value;

pub(crate) struct StructCol {
    pub(crate) name: String,
    pub(crate) ty: String,
    pub(crate) notnull: bool,
    pub(crate) dflt: Option<String>,
    pub(crate) pk: bool,
}

pub(crate) struct TableStructure {
    pub(crate) cols: Vec<StructCol>,
    pub(crate) ddl: Option<String>,
}

/// Fetch a table's structure. Runs on the background thread alongside the
/// first page (fetch first, commit over the old value), so switching to
/// the Structure view later is instant.
pub(crate) fn table_structure(
    conn: &Conn,
    schema: &str,
    name: &str,
) -> Option<TableStructure> {
    let qualified =
        format!("{}.{}", crate::util::qident(schema), crate::util::qident(name));
    let sql = format!("PRAGMA table_info('{}')", qualified.replace('\'', "''"));
    let info = harbor_client::query(conn, &sql).ok()?;
    let pos = |key: &str| info.columns.iter().position(|c| c.name.as_deref() == Some(key));
    let (name_ix, ty_ix, notnull_ix, dflt_ix, pk_ix) =
        (pos("name")?, pos("type")?, pos("notnull")?, pos("dflt_value")?, pos("pk")?);
    let text = |v: Option<&Value>| match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    };
    let cols = info
        .rows
        .iter()
        .map(|row| StructCol {
            name: text(row.get(name_ix)),
            ty: text(row.get(ty_ix)),
            notnull: row.get(notnull_ix) == Some(&Value::Bool(true)),
            dflt: match row.get(dflt_ix) {
                Some(Value::Null) | None => None,
                Some(v) => Some(text(Some(v))),
            },
            pk: row.get(pk_ix) == Some(&Value::Bool(true)),
        })
        .collect();

    let ddl_sql = format!(
        "SELECT sql FROM duckdb_tables() WHERE schema_name = '{}' AND table_name = '{}'",
        schema.replace('\'', "''"),
        name.replace('\'', "''"),
    );
    let ddl = harbor_client::query(conn, &ddl_sql)
        .ok()
        .and_then(|r| r.rows.first()?.first().cloned())
        .and_then(|v| match v {
            Value::String(s) => Some(s),
            _ => None,
        });

    Some(TableStructure { cols, ddl })
}

const ROW_H: f32 = 26.;
const NAME_W: f32 = 220.;
const TYPE_W: f32 = 180.;
const ATTR_W: f32 = 130.;

impl Grid {
    pub(crate) fn structure_view(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = pal(cx);
        let mut pane = div()
            .id("structure")
            .v_flex()
            .size_full()
            .px_3()
            .py_2()
            .overflow_y_scroll();
        let Some(s) = self.structure() else {
            return pane.child(
                div().text_sm().text_color(t.muted).child("Structure unavailable"),
            );
        };

        pane = pane.child(header_row(t));
        for c in &s.cols {
            pane = pane.child(
                div()
                    .h_flex()
                    .h(px(ROW_H))
                    .flex_none()
                    .items_center()
                    .gap_2()
                    .border_b_1()
                    .border_color(t.grid_line)
                    .child(cell(t, &c.name, NAME_W, false))
                    .child(cell(t, &c.ty, TYPE_W, true))
                    .child(
                        div()
                            .h_flex()
                            .w(px(ATTR_W))
                            .flex_none()
                            .gap_1()
                            .when(c.pk, |d| d.child(chip(t, "PK", true)))
                            .when(c.notnull, |d| d.child(chip(t, "NOT NULL", false))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .font_family(value_font())
                            .text_color(t.muted)
                            .child(c.dflt.clone().unwrap_or_default()),
                    ),
            );
        }

        if let Some(ddl) = &s.ddl {
            pane = pane.child(
                div()
                    .pt_4()
                    .pb_1()
                    .text_size(px(10.5))
                    .font_weight(FontWeight::BOLD)
                    .text_color(t.muted)
                    .child("DDL"),
            );
            pane = pane.child(
                div()
                    .p_2()
                    .rounded(px(6.))
                    .bg(t.raised)
                    .border_1()
                    .border_color(t.grid_line)
                    .text_size(px(11.5))
                    .font_family(value_font())
                    .text_color(t.text)
                    .child(ddl.clone()),
            );
        }

        pane
    }
}

fn header_row(t: Pal) -> impl IntoElement {
    let th = |label: &'static str, w: f32| {
        div()
            .w(px(w))
            .flex_none()
            .text_size(px(11.5))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(t.text)
            .child(label)
    };
    div()
        .h_flex()
        .h(px(ROW_H))
        .flex_none()
        .items_center()
        .gap_2()
        .border_b_1()
        .border_color(t.border)
        .child(th("column", NAME_W))
        .child(th("type", TYPE_W))
        .child(th("attributes", ATTR_W))
        .child(
            div()
                .flex_1()
                .text_size(px(11.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(t.text)
                .child("default"),
        )
}

fn cell(t: Pal, text: &str, w: f32, muted: bool) -> impl IntoElement {
    div()
        .w(px(w))
        .flex_none()
        .truncate()
        .text_size(px(12.))
        .font_family(value_font())
        .text_color(if muted { t.muted } else { t.text })
        .child(text.to_string())
}

fn chip(t: Pal, label: &'static str, accent: bool) -> impl IntoElement {
    div()
        .flex_none()
        .px(px(5.))
        .rounded(px(4.))
        .text_size(px(10.))
        .font_family(ui_font())
        .map(|d| {
            if accent {
                d.bg(t.accent.opacity(0.15)).text_color(t.accent)
            } else {
                d.bg(t.grid_line.opacity(0.55)).text_color(t.muted)
            }
        })
        .child(label)
}
