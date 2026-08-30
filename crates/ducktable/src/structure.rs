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
            Value::String(s) => Some(pretty_ddl(&s)),
            _ => None,
        });

    Some(TableStructure { cols, ddl })
}

/// Reformat DuckDB's one-line CREATE TABLE into an indented definition:
/// one column per line, names padded so the types align. Splits only at
/// depth-0 commas, so `DEFAULT(...)` and `DECIMAL(10,2)` stay intact;
/// quoted identifiers and strings are opaque to the scan.
pub(crate) fn pretty_ddl(sql: &str) -> String {
    let sql = sql.trim();
    let Some(open) = top_level_open(sql) else {
        return sql.to_string();
    };
    let head = sql[..open].trim_end();
    let tail = &sql[open + 1..];
    let mut depth = 0usize;
    let (mut in_s, mut in_d) = (false, false);
    let mut defs: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut rest = "";
    for (i, ch) in tail.char_indices() {
        if in_s {
            if ch == '\'' {
                in_s = false;
            }
            cur.push(ch);
            continue;
        }
        if in_d {
            if ch == '"' {
                in_d = false;
            }
            cur.push(ch);
            continue;
        }
        match ch {
            '\'' => {
                in_s = true;
                cur.push(ch);
            }
            '"' => {
                in_d = true;
                cur.push(ch);
            }
            '(' => {
                depth += 1;
                cur.push(ch);
            }
            ')' if depth == 0 => {
                rest = tail[i..].trim();
                break;
            }
            ')' => {
                depth -= 1;
                cur.push(ch);
            }
            ',' if depth == 0 => defs.push(std::mem::take(&mut cur)),
            _ => cur.push(ch),
        }
    }
    if !cur.trim().is_empty() {
        defs.push(cur);
    }
    let defs: Vec<String> = defs.iter().map(|d| d.trim().to_string()).collect();
    // Pad column names so the types line up; table-level constraints
    // (PRIMARY KEY (...), UNIQUE (...), FOREIGN KEY ...) go unpadded.
    let name_len = |d: &str| -> Option<usize> {
        let n = if d.starts_with('"') {
            d[1..].find('"').map(|e| e + 2)?
        } else {
            d.find(char::is_whitespace)?
        };
        const TABLE_LEVEL: [&str; 5] =
            ["PRIMARY", "UNIQUE", "FOREIGN", "CHECK", "CONSTRAINT"];
        (!TABLE_LEVEL.contains(&d[..n].to_uppercase().as_str())).then_some(n)
    };
    let width = defs.iter().filter_map(|d| name_len(d)).max().unwrap_or(0);
    let mut out = String::with_capacity(sql.len() + defs.len() * 4);
    out.push_str(head);
    out.push_str(" (\n");
    for (i, d) in defs.iter().enumerate() {
        out.push_str("  ");
        match name_len(d) {
            Some(n) => {
                out.push_str(&d[..n]);
                for _ in n..width + 1 {
                    out.push(' ');
                }
                out.push_str(d[n..].trim_start());
            }
            None => out.push_str(d),
        }
        if i + 1 < defs.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str(rest);
    out
}

/// The first '(' outside any quote — where the column list begins.
fn top_level_open(sql: &str) -> Option<usize> {
    let (mut in_s, mut in_d) = (false, false);
    for (i, ch) in sql.char_indices() {
        match ch {
            '\'' if !in_d => in_s = !in_s,
            '"' if !in_s => in_d = !in_d,
            '(' if !in_s && !in_d => return Some(i),
            _ => {}
        }
    }
    None
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
            let ddl_text = ddl.clone();
            pane = pane.child(
                div()
                    .h_flex()
                    .items_center()
                    .pt_4()
                    .pb_1()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(10.5))
                            .font_weight(FontWeight::BOLD)
                            .text_color(t.muted)
                            .child("DDL"),
                    )
                    .child(
                        crate::grid::icon_tile("copy-ddl", 20., true, t)
                            .tooltip(|window, cx| {
                                gpui_component::tooltip::Tooltip::new("Copy DDL")
                                    .build(window, cx)
                            })
                            .child(
                                // Raw svg() does NOT inherit text color.
                                svg()
                                    .path("icons/copy.svg")
                                    .size_3()
                                    .text_color(t.muted),
                            )
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    ddl_text.clone(),
                                ));
                            }),
                    ),
            );
            pane = pane.child(match &self.ddl_input {
                // A disabled Input keeps native text selection (drag,
                // Cmd+C) while gating off every mutation — read-only
                // selectable text, which gpui divs cannot give. Styles
                // go ON the Input: it overrides the inherited text size
                // (input_text_size), and only its own refinement, which
                // applies last, beats that. Same 12px value font as the
                // columns table above.
                Some(state) => gpui_component::input::Input::new(state)
                    .disabled(true)
                    .text_size(px(12.))
                    .font_family(value_font())
                    .into_any_element(),
                None => div()
                    .p_2()
                    .rounded(px(6.))
                    .bg(t.raised)
                    .border_1()
                    .border_color(t.grid_line)
                    .text_size(px(11.5))
                    .font_family(value_font())
                    .text_color(t.text)
                    .child(ddl.clone())
                    .into_any_element(),
            });
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

#[cfg(test)]
mod tests {
    use super::pretty_ddl;

    #[test]
    fn ddl_formats_one_column_per_line() {
        assert_eq!(
            pretty_ddl("CREATE TABLE t(a INTEGER, b VARCHAR);"),
            "CREATE TABLE t (\n  a INTEGER,\n  b VARCHAR\n);"
        );
    }

    #[test]
    fn ddl_keeps_nested_commas_and_pads_names() {
        let raw = "CREATE TABLE partners(id INTEGER DEFAULT(nextval('id')) PRIMARY KEY, \
                   eid VARCHAR NOT NULL UNIQUE, \
                   created_at TIMESTAMP DEFAULT(timezone('UTC', now())));";
        let out = pretty_ddl(raw);
        assert!(out.starts_with("CREATE TABLE partners (\n"));
        // The DEFAULT's inner comma did not split the line.
        assert!(out.contains("DEFAULT(timezone('UTC', now()))"));
        // Names pad to the longest (created_at), so both types start at
        // the same column.
        let col = |name: &str| {
            out.lines().find(|l| l.trim_start().starts_with(name)).unwrap().find("INTEGER")
                .or_else(|| {
                    out.lines().find(|l| l.trim_start().starts_with(name)).unwrap().find("VARCHAR")
                })
                .unwrap()
        };
        assert_eq!(col("id"), col("eid"));
        assert!(out.ends_with(");"));
    }

    #[test]
    fn ddl_leaves_table_level_constraints_unpadded() {
        let out = pretty_ddl("CREATE TABLE t(a INTEGER, PRIMARY KEY (a));");
        assert!(out.contains("\n  PRIMARY KEY (a)\n"));
    }
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
