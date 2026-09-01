//! The Structure view: the table's columns and DDL (UI.md "Bottom bar"),
//! swapped in for the data grid via the footer's view switcher. Viewing
//! data and viewing schema are exclusive by design — a schema change
//! reshapes the data view, so the two never render side by side. Read-only
//! in this slice; the editor arrives with the staged/live pipeline
//! (edits.rs).
//!
//! Everything shown here is a projection of the `/catalog` document —
//! columns, constraints, and (harbor 0.18+) the engine's own CREATE TABLE
//! text — so opening the Structure view costs no query at all.

use crate::grid::Grid;
use crate::theme::{pal, value_font, CELL_TEXT, PANE_INSET};
use gpui::*;
use gpui_component::StyledExt as _;

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

/// A table's structure, read straight out of the catalog snapshot the
/// connection already holds — no query, so the Structure view is as
/// current as the sidebar and never a frame behind it.
pub(crate) fn table_structure(table: &harbor_client::Table) -> TableStructure {
    let cols = table
        .columns
        .iter()
        .map(|c| StructCol {
            name: c.name.clone(),
            ty: c.duck_type.clone(),
            notnull: c.not_null,
            dflt: c.default.clone(),
            pk: c.primary,
        })
        .collect();
    TableStructure { cols, ddl: table.ddl.as_deref().map(pretty_ddl) }
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
    let mut rest = None;
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
                rest = Some(tail[i..].trim());
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
    // No closing paren means this was never the definition list the scan
    // assumed; hand back the original rather than half a reformat.
    let Some(rest) = rest else {
        return sql.to_string();
    };
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

/// The pane width, recorded by the title strip's canvas every frame in
/// every view — APP-STATIC, not per-grid, so it survives the grid swap
/// a table switch performs and the DDL's first frame always knows its
/// width. One window, one pane, one cell.
static PANE_WIDTH: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub(crate) fn record_pane_width(w: Pixels) {
    PANE_WIDTH.store(f32::from(w).to_bits(), std::sync::atomic::Ordering::Relaxed);
}

fn pane_width() -> f32 {
    f32::from_bits(PANE_WIDTH.load(std::sync::atomic::Ordering::Relaxed))
}

/// The columns table AS a grid: the table's own schema synthesized
/// into a result and handed to the real Grid — one grid, many sources,
/// applied to the catalog itself. Embedded, unpageable, read-only by
/// construction; the attributes column renders its tags as pills
/// (grid.rs pill_cols). Costs no query: the catalog snapshot is
/// already in hand.
pub(crate) fn columns_grid(
    conn: harbor_client::Conn,
    s: &TableStructure,
    window: &mut Window,
    cx: &mut Context<Grid>,
) -> Entity<Grid> {
    let columns = ["column", "type", "attributes", "default"]
        .iter()
        .map(|n| wire::Column {
            name: Some((*n).to_string()),
            duckdb_type: "VARCHAR".to_string(),
            lossless: true,
            ..Default::default()
        })
        .collect();
    let rows: Vec<Vec<serde_json::Value>> = s
        .cols
        .iter()
        .map(|c| {
            let attrs = match (c.pk, c.notnull) {
                (true, true) => "PK \u{00b7} NOT NULL",
                (true, false) => "PK",
                (false, true) => "NOT NULL",
                (false, false) => "",
            };
            vec![
                c.name.clone().into(),
                c.ty.clone().into(),
                attrs.into(),
                c.dflt.clone().unwrap_or_default().into(),
            ]
        })
        .collect();
    let n = rows.len();
    let result = harbor_client::QueryResult {
        columns,
        rows,
        row_count: n as u64,
        time_ms: 0,
    };
    let grid = cx.new(|cx| {
        Grid::new_query(
            conn,
            "structure",
            Ok(result),
            Some(n as u64),
            n.max(1),
            false,
            window,
            cx,
        )
    });
    grid.update(cx, |g, cx| {
        g.mark_pill_column(2, cx);
        // Sized to content, like the old fixed-width table — but
        // resizable and fittable now, because it is a real grid.
        g.fit_columns(cx);
    });
    grid
}

impl Grid {
    pub(crate) fn structure_view(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let t = pal(cx);
        let z = crate::prefs::get(cx).zoom_factor();
        let pane = div().id("structure").v_flex().size_full();
        let Some(grid) = self.structure_grid.clone() else {
            return pane.pl(px(PANE_INSET)).py_2().child(
                div().text_sm().text_color(t.muted).child("Structure unavailable"),
            );
        };
        let n = self.structure().map(|s| s.cols.len()).unwrap_or(0);
        let row_h = f32::from(crate::prefs::get(cx).table_size().table_row_height());
        // The grid's full content height: rows + header + borders.
        let content_h = row_h * (n + 1) as f32 + 2.;

        // ddl and ddl_input come from the same source in Grid::build, so
        // they are Some together.
        let ddl_section = if let (Some(state), Some(copy)) = (&self.ddl_input, &self.ddl_copy) {
            let mut ddl = div().v_flex().flex_none().pl(px(PANE_INSET)).pr_3();
            ddl = ddl.child(
                div()
                    .h_flex()
                    .items_center()
                    // 12px above matches PANE_INSET on the left: the DDL
                    // section gets the same breathing room on both axes.
                    .pt_3()
                    .pb_1()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(10.5))
                            .font_weight(FontWeight::BOLD)
                            .text_color(t.muted)
                            .child("DDL"),
                    )
                    // The copy tile is its own little machine (flash, timer,
                    // clipboard) — see copy_button.rs.
                    .child(copy.clone()),
            );
            // A disabled Input keeps native text selection (drag, Cmd+C)
            // while gating off every mutation — read-only selectable
            // text, which gpui divs cannot give. The card sits EXACTLY
            // content-sized and cannot scroll: line height is pinned ON
            // the input so rows × line height IS the content height,
            // and the vendored gpui-component carries the "a disabled
            // editor never scrolls itself" patch (no scroll-past-end
            // room, no cursor-track nudges — see vendor/, Cargo.toml
            // [patch]). The wheel falls through to the pane naturally.
            let line_h = 20. * z;
            // Shrink-wrapped: the card is as wide as its longest line
            // (7/11 is Menlo's advance-to-size ratio, the constant the
            // gutter already trusts) and only ever stretches to the
            // pane's width, where long lines soft-wrap.
            let max_chars = state
                .read(cx)
                .value()
                .lines()
                .map(|l| l.chars().count())
                .max()
                .unwrap_or(0)
                .max(8);
            let mut card_w = max_chars as f32 * (CELL_TEXT * z * 7. / 11.) + 28.;
            // Cap at the pane width recorded LAST frame (the title
            // strip's canvas — present in every view, so it is already
            // known before the Structure view's first paint), and seed
            // the editor's wrap width with the exact value its layout
            // will derive: card − borders(2) − input_px(16) −
            // RIGHT_MARGIN(10). The wrapper re-wraps synchronously, so
            // the row count read below is FIRST-FRAME correct — drawn
            // right, not repainted right (Steve's ruling). A cold start
            // straight into Structure has no recorded width yet and
            // settles via the editor observer instead.
            let avail = pane_width() - PANE_INSET - 12.;
            if avail > 60. {
                card_w = card_w.min(avail);
                // Font AND width: the wrapper is born with the app's
                // default font at rem size, and only learns the card's
                // real metrics (Menlo at CELL_TEXT × zoom) during
                // layout — a width-only seed wraps at the wrong
                // boundaries and still flickers.
                state.update(cx, |s, cx| {
                    s.prewrap(
                        font(value_font()),
                        px(CELL_TEXT * z),
                        px(card_w - 28.),
                        cx,
                    );
                });
            }
            // WRAPPED rows, from the editor's own wrapper (a vendored
            // accessor): the card's height must count soft-wrapped rows
            // — everything visible, nothing to scroll.
            let rows = state
                .read(cx)
                .wrapped_line_count()
                .max(state.read(cx).value().lines().count())
                .max(2);
            // The frame is OURS, from the app palette — the component's
            // own border/fill are stock colors that ignore the theme
            // (Paper's DDL wore a blue box). appearance(false) strips
            // them; the wrapper draws the card in t.raised/t.border,
            // correct in every theme by construction.
            ddl = ddl.child(
                div()
                    .rounded(px(6.))
                    .border_1()
                    .border_color(t.border)
                    .bg(t.raised)
                    .py_1()
                    .mb_2()
                    .w(px(card_w))
                    .max_w_full()
                    .child(
                        gpui_component::input::Input::new(state)
                            .disabled(true)
                            .appearance(false)
                            .h(px(rows as f32 * line_h + 14.))
                            .text_size(px(CELL_TEXT * z))
                            .line_height(px(line_h))
                            .font_family(value_font()),
                    ),
            );
            Some(ddl)
        } else {
            None
        };

        if let (Some(ddl), Some(split)) = (ddl_section, &self.structure_split) {
            // Two panes, one divider — the user's, like the Query
            // view's editor/results split: draggable, persisted, the
            // handle's own line marking where the columns end and the
            // DDL begins. The columns grid scrolls internally above it;
            // the DDL scrolls below it.
            let pref = crate::prefs::get(cx).structure_split;
            // Auto (never dragged): the classic ~20-row cap (Steve's
            // ruling, 2026-08-31), landing mid-row when clipped so a
            // half-visible row 21 says "cut, not end".
            let auto = if n > 20 { row_h * 21.5 + 2. } else { content_h };
            let want = if pref > 0. { pref } else { auto };
            // Never taller than the grid's content — a small table
            // stays shrink-wrapped, its DDL right below — and never
            // squashed past the floor content itself allows.
            let lo = crate::prefs::STRUCTURE_SPLIT_MIN.min(content_h);
            let h = want.min(content_h).max(lo);
            pane.child(
                gpui_component::resizable::v_resizable("structure-split")
                    .with_state(split)
                    .child(
                        gpui_component::resizable::resizable_panel()
                            .size(px(h))
                            .size_range(px(lo)..px(crate::prefs::STRUCTURE_SPLIT_MAX))
                            // Furniture: only the user's drag moves the
                            // divider — a window resize gives its delta
                            // to the DDL.
                            .fixed()
                            .child(div().size_full().min_h_0().child(grid)),
                    )
                    .child(
                        gpui_component::resizable::resizable_panel().child(
                            div()
                                .id("structure-ddl")
                                .size_full()
                                .min_h_0()
                                .v_flex()
                                .overflow_y_scroll()
                                .child(ddl),
                        ),
                    ),
            )
        } else {
            // No DDL, so no divider: the grid alone, capped at ~20 rows
            // (it scrolls internally for the rest), the pane scrolling
            // as one. +1 for the header row, +2 for the borders.
            let cap = row_h * (n.min(20) + 1) as f32 + 2.;
            pane.overflow_y_scroll()
                .child(div().flex_none().w_full().h(px(cap)).child(grid))
        }
    }
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

    #[test]
    fn ddl_without_a_closing_paren_passes_through_verbatim() {
        let raw = "CREATE TABLE broken(a INTEGER, b VARCHAR";
        assert_eq!(pretty_ddl(raw), raw);
    }
}
