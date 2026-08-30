//! The content pane: table structure, berth identity, and the connect
//! flow's states (idle, connecting with cancel, failed with retry).

use crate::app::{DuckTable, Phase};
use crate::theme::{pal, Pal};
use crate::util::clone_str;
use gpui::*;
use gpui_component::button::*;
use gpui_component::*;
use harbor_client::Level;

impl DuckTable {
    pub(crate) fn content(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = pal(cx);
        if let (Phase::Connected { .. }, Some(grid)) = (&self.phase, &self.grid) {
            return div()
                .flex_1()
                .min_w_0()
                .h_full()
                .bg(t.surface)
                .child(grid.clone())
                .into_any_element();
        }
        let body = match &self.phase {
            Phase::Idle => div()
                .v_flex()
                .gap_2()
                .items_center()
                .child(div().text_lg().text_color(t.text).child("DuckTable"))
                .child(
                    div()
                        .text_sm()
                        .text_color(t.muted)
                        .child("Pick a berth on the left. A stopped berth with a path spawns on demand."),
                ),
            Phase::Connecting { name } => div()
                .v_flex()
                .gap_3()
                .items_center()
                .child(
                    div()
                        .text_sm()
                        .text_color(t.muted)
                        .child(format!("Connecting to {name}...")),
                )
                .child(
                    Button::new("cancel")
                        .label("Cancel")
                        .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                ),
            Phase::Failed { name, message } => div()
                .v_flex()
                .gap_3()
                .items_center()
                .child(
                    div()
                        .text_sm()
                        .text_color(t.bad)
                        .child(format!("Couldn't connect to {name}")),
                )
                .child(div().text_xs().text_color(t.muted).child(clone_str(message)))
                .child({
                    let name = clone_str(name);
                    Button::new("retry").label("Retry").primary().on_click(cx.listener(
                        move |this, _, _, cx| this.connect(clone_str(&name), cx),
                    ))
                }),
            Phase::Connected { conn, info, .. } => div()
                .v_flex()
                .gap_1()
                .items_start()
                .p_4()
                .min_w(px(440.))
                .max_w_full()
                .overflow_hidden()
                .bg(t.surface)
                .border_1()
                .border_color(t.border)
                .rounded_lg()
                .child(
                    div()
                        .h_flex()
                        .gap_2()
                        .items_center()
                        .child(Self::dot(Level::Good, t))
                        .child(div().text_lg().text_color(t.text).child(clone_str(&info.name))),
                )
                .child(meta(t, "DuckDB", clone_str(&info.duckdb_version)))
                .child(meta(t, "Harbor", clone_str(&info.harbor_version)))
                .child(meta(
                    t,
                    "Database",
                    harbor_client::paths::shorten(std::path::Path::new(&info.database)),
                ))
                .child(meta(t, "Uptime", format!("{}s", info.uptime_ms / 1000)))
                .child(meta(
                    t,
                    "Lifetime",
                    if conn.summoned { "summoned by this window".into() } else { "joined, already running".into() },
                )),
        };
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(t.surface)
            .v_flex()
            .items_center()
            .justify_center()
            .p_6()
            .child(body)
            .into_any_element()
    }
}

fn meta(t: Pal, k: &'static str, v: String) -> impl IntoElement {
    div()
        .h_flex()
        .gap_2()
        .w_full()
        .text_sm()
        .child(div().w_20().flex_none().text_color(t.muted).child(k))
        .child(div().flex_1().min_w_0().truncate().text_color(t.text).child(v))
}
