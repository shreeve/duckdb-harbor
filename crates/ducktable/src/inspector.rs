//! The inspector: the grid's right panel (design/main-window.html, UI.md
//! "Inspector"). It lives INSIDE the grid, below the header strip — the
//! title/toggles row keeps the full width and nothing shifts when the
//! panel opens. It shows ROW-LEVEL data only: the selected row's values,
//! vertically. Berth facts (versions, size) live on the identity card and
//! row counts in the grid's status line — different data urgency levels
//! never share this pane. Read-only in this slice; the row editor arrives
//! with the staged/live pipeline (edits.rs) — it and the grid's inline
//! editor will be one editing session with one owner.

use crate::grid::Grid;
use crate::theme::{pal, value_font, Pal};
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::StyledExt as _;

impl Grid {
    pub(crate) fn inspector(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = pal(cx);

        let mut pane = div()
            .id("inspector")
            .v_flex()
            // Wide enough that a full microsecond timestamp
            // (26 mono chars) survives the key column and padding.
            .w(px(290.))
            .flex_none()
            .h_full()
            .px_3()
            .py_2()
            // Raised, like the proof's inspector pane — not another white
            // panel against the white grid.
            .bg(t.raised)
            .border_l_1()
            .border_color(t.border)
            .overflow_y_scroll()
            .child(section(t, "ROW"));

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
        .pt_1()
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
