//! The content pane: table structure, berth identity, and the connect
//! flow's states (idle, connecting with cancel, failed with retry).

use crate::app::{DuckTable, Phase};
use crate::theme::*;
use crate::util::clone_str;
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::button::*;
use gpui_component::*;
use harbor_client::Level;

impl DuckTable {
    pub(crate) fn content(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match &self.phase {
            Phase::Idle => div()
                .v_flex()
                .gap_2()
                .items_center()
                .child(div().text_lg().text_color(rgb(TEXT)).child("DuckTable"))
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(MUTED))
                        .child("Pick a berth on the left. A stopped berth with a path spawns on demand."),
                ),
            Phase::Connecting { name } => div()
                .v_flex()
                .gap_3()
                .items_center()
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(MUTED))
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
                        .text_color(rgb(BAD))
                        .child(format!("Couldn't connect to {name}")),
                )
                .child(div().text_xs().text_color(rgb(MUTED)).child(clone_str(message)))
                .child({
                    let name = clone_str(name);
                    Button::new("retry").label("Retry").primary().on_click(cx.listener(
                        move |this, _, _, cx| this.connect(clone_str(&name), cx),
                    ))
                }),
            Phase::Connected { catalog, .. }
                if self.selected_table.as_ref().is_some_and(|(s, n)| {
                    catalog.tables.iter().any(|t| &t.schema == s && &t.name == n)
                }) =>
            {
                let (schema, name) = self.selected_table.clone().unwrap();
                let table = catalog
                    .tables
                    .iter()
                    .find(|t| t.schema == schema && t.name == name)
                    .unwrap();
                div()
                    .v_flex()
                    .gap_1()
                    .items_start()
                    .p_4()
                    .min_w(px(440.))
                    .max_w_full()
                    .max_h_full()
                    .overflow_hidden()
                    .bg(rgb(BG_SURFACE))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .rounded_lg()
                    .child(
                        div()
                            .text_lg()
                            .text_color(rgb(TEXT))
                            .child(format!("{schema}.{name}")),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .pb_2()
                            .child(format!("{} columns", table.columns.len())),
                    )
                    .children(table.columns.iter().map(|c| {
                        div()
                            .h_flex()
                            .gap_2()
                            .w_full()
                            .text_sm()
                            .child(
                                div()
                                    .w_48()
                                    .flex_none()
                                    .truncate()
                                    .text_color(rgb(TEXT))
                                    .child(clone_str(&c.name)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(rgb(MUTED))
                                    .child(clone_str(&c.duck_type)),
                            )
                            .when(c.primary, |d| {
                                d.child(div().text_xs().text_color(rgb(ACCENT)).child("PK"))
                            })
                            .when(c.not_null && !c.primary, |d| {
                                d.child(div().text_xs().text_color(rgb(MUTED)).child("NOT NULL"))
                            })
                    }))
            }
            Phase::Connected { conn, info, .. } => div()
                .v_flex()
                .gap_1()
                .items_start()
                .p_4()
                .min_w(px(440.))
                .max_w_full()
                .overflow_hidden()
                .bg(rgb(BG_SURFACE))
                .border_1()
                .border_color(rgb(BORDER))
                .rounded_lg()
                .child(
                    div()
                        .h_flex()
                        .gap_2()
                        .items_center()
                        .child(Self::dot(Level::Good))
                        .child(div().text_lg().text_color(rgb(TEXT)).child(clone_str(&info.name))),
                )
                .child(meta("DuckDB", clone_str(&info.duckdb_version)))
                .child(meta("Harbor", clone_str(&info.harbor_version)))
                .child(meta(
                    "Database",
                    harbor_client::paths::shorten(std::path::Path::new(&info.database)),
                ))
                .child(meta("Uptime", format!("{}s", info.uptime_ms / 1000)))
                .child(meta(
                    "Lifetime",
                    if conn.summoned { "summoned by this window".into() } else { "joined, already running".into() },
                )),
        };
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(rgb(BG_SURFACE))
            .v_flex()
            .items_center()
            .justify_center()
            .p_6()
            .child(body)
    }
}

fn meta(k: &'static str, v: String) -> impl IntoElement {
    div()
        .h_flex()
        .gap_2()
        .w_full()
        .text_sm()
        .child(div().w_20().flex_none().text_color(rgb(MUTED)).child(k))
        .child(div().flex_1().min_w_0().truncate().text_color(rgb(TEXT)).child(v))
}
