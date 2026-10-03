//! The inspector: the grid's right panel (design/main-window.html, DESIGN.md
//! "Inspector"). It lives INSIDE the grid, below the header strip — the
//! title/toggles row keeps the full width and nothing shifts when the
//! panel opens. It shows ROW-LEVEL data only: the selected row's values,
//! vertically. Berth facts (versions, size) live on the identity card and
//! row counts in the grid's status line — different data urgency levels
//! never share this pane. Read-only: a row editor here is planned (DESIGN.md),
//! and would share the grid's one editing session (edits.rs).

use crate::grid::Grid;
use crate::theme::{pal, value_font, Pal, CELL_TEXT, HEADER_TEXT};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use gpui_kit::component::StyledExt as _;

impl Grid {
    pub(crate) fn inspector(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let t = pal(cx);
        let z = crate::prefs::get(cx).zoom_factor();

        // The width belongs to the resizable panel wrapping this pane
        // (grid.rs), so the pane just fills it.
        let mut list = div()
            .id("inspector")
            .v_flex()
            .size_full()
            .px_3()
            .py_2()
            .overflow_y_scroll()
            .child(section(t, "ROW"));

        match self.row_kv(cx) {
            Some(pairs) => {
                for (k, v, is_null) in pairs {
                    list = list.child(kv(t, z, k, v, is_null));
                }
            }
            None => {
                list = list.child(
                    div()
                        .pt_1()
                        .text_size(px(CELL_TEXT * z))
                        .text_color(t.muted)
                        .child("Select a row"),
                );
            }
        }

        div()
            .size_full()
            // Raised, like the proof's inspector pane — not another white
            // panel against the white grid.
            .bg(t.raised)
            .border_l_1()
            .border_color(t.border)
            .child(list)
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

fn kv(t: Pal, z: f32, k: SharedString, v: SharedString, is_null: bool) -> impl IntoElement {
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
                .max_w(px(110. * z))
                .truncate()
                .text_size(px(CELL_TEXT * z))
                .text_color(t.muted)
                .child(k),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(px(HEADER_TEXT * z))
                .font_family(value_font())
                .text_color(if is_null { t.muted } else { t.text })
                .when(is_null, |d| d.italic())
                .child(v),
        )
}
