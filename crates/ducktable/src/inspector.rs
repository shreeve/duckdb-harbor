//! The inspector: the grid's right panel (design/main-window.html, UI.md
//! "Inspector"). It lives INSIDE the grid, below the header strip — the
//! title/toggles row keeps the full width and nothing shifts when the
//! panel opens. Read-only in this slice — SIZE, STATISTICS, and METADATA
//! from the berth, and ROW showing the selected row's values vertically.
//! The row editor arrives with the staged/live pipeline (edits.rs); it and
//! the grid's inline editor will be one editing session with one owner.

use crate::grid::Grid;
use crate::theme::{pal, value_font, Pal};
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::StyledExt as _;

/// The berth facts the inspector shows. Fixed for a connection's lifetime,
/// so they ride into the grid at construction instead of reaching back
/// into the app's phase.
pub(crate) struct BerthMeta {
    pub(crate) berth: String,
    pub(crate) duckdb: String,
    pub(crate) harbor: String,
    /// `PRAGMA database_size` (database_size, wal_size), as the server
    /// prints them.
    pub(crate) db_size: Option<(String, String)>,
}

impl Grid {
    pub(crate) fn inspector(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = pal(cx);
        let meta = self.meta();

        let mut pane = div()
            .id("inspector")
            .v_flex()
            .w(px(260.))
            .flex_none()
            .h_full()
            .px_3()
            .py_2()
            .bg(t.surface)
            .border_l_1()
            .border_color(t.border)
            .overflow_y_scroll();

        if let Some((data, wal)) = meta.db_size.clone() {
            pane = pane
                .child(section(t, "SIZE"))
                .child(kv(t, "Data", data, false))
                .child(kv(t, "WAL", wal, false));
        }

        let (loaded, total) = self.counts(cx);
        pane = pane.child(section(t, "STATISTICS")).child(kv(
            t,
            "Rows",
            match total {
                Some(n) => format!("{n}"),
                None => format!("{loaded}+"),
            },
            false,
        ));

        pane = pane
            .child(section(t, "METADATA"))
            .child(kv(t, "DuckDB", meta.duckdb.clone(), false))
            .child(kv(t, "Harbor", meta.harbor.clone(), false))
            .child(kv(t, "Berth", meta.berth.clone(), false));

        pane = pane.child(section(t, "ROW"));
        match self.row_kv(cx) {
            Some(pairs) => {
                for (k, v, is_null) in pairs {
                    pane = pane.child(kv(t, k, v, is_null));
                }
            }
            None => {
                pane = pane.child(
                    div()
                        .pt_1()
                        .text_size(px(12.))
                        .text_color(t.muted)
                        .child("Select a row"),
                );
            }
        }

        pane
    }
}

fn section(t: Pal, label: &'static str) -> impl IntoElement {
    div()
        .pt_3()
        .pb_1()
        .text_size(px(10.5))
        .font_weight(FontWeight::BOLD)
        .text_color(t.muted)
        .child(label)
}

fn kv(t: Pal, k: impl Into<SharedString>, v: String, is_null: bool) -> impl IntoElement {
    div()
        .h_flex()
        .justify_between()
        .gap_2()
        .py(px(3.))
        .border_b_1()
        .border_color(t.grid_line)
        .child(
            div()
                .flex_none()
                .max_w(px(110.))
                .truncate()
                .text_size(px(12.))
                .text_color(t.muted)
                .child(k.into()),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(px(11.5))
                .font_family(value_font())
                .text_color(if is_null { t.muted } else { t.text })
                .when(is_null, |d| d.italic())
                .child(v),
        )
}
