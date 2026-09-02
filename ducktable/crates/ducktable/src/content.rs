//! The content pane: table structure, berth identity, and the connect
//! flow's states (idle, connecting with cancel, failed with retry).

use crate::app::{DuckTable, Phase};
use crate::theme::{pal, Pal};
use gpui::prelude::FluentBuilder as _;
use crate::util::clone_str;
use gpui::*;
use gpui_component::button::*;
use gpui_component::*;
use harbor_client::Level;

impl DuckTable {
    pub(crate) fn content(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
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
        // While a connect is in flight, whatever is on screen keeps
        // rendering and the pane swaps in one frame when the outcome lands.
        // Only the idle and failed cards give way to a connecting card —
        // they hold nothing worth preserving, and it carries the cancel
        // affordance a cold summon needs.
        let in_flight = match &self.phase {
            Phase::Connected { .. } => None,
            _ => self.connecting.as_deref(),
        };
        let body = if let Some(name) = in_flight {
            div()
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
                )
        } else {
            match &self.phase {
            Phase::Idle => div()
                .v_flex()
                .gap_2()
                .items_center()
                .child(div().text_lg().text_color(t.text).child("DuckTable"))
                .child(
                    div()
                        .text_sm()
                        .text_color(t.muted)
                        .child("Pick a database on the left. A stopped database starts on demand."),
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
            Phase::Connected { conn, info, catalog } => {
                let installed = self.installed_version.clone();
                let stale = installed.as_deref().is_some_and(|iv| {
                    harbor_client::fleet::version_older(&info.harbor_version, iv)
                });
                // A local server (its row carries a path) can be restarted from
                // here; a remote one can only be noted as behind.
                let is_local = self
                    .rows
                    .iter()
                    .find(|r| r.name == info.name)
                    .is_some_and(|r| r.path.is_some());
                div()
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
                .child(harbor_meta(t, &info.harbor_version, stale, is_local, installed.as_deref()))
                // The path row is the one worth copying (paste into a shell,
                // a bug report, another tool), so it carries the same
                // self-confirming copy tile the DDL block uses — painted
                // labels have no OS text-selection, so this is the way out.
                .child(
                    div()
                        .h_flex()
                        .gap_2()
                        .w_full()
                        .items_center()
                        .text_sm()
                        .child(div().w_20().flex_none().text_color(t.muted).child("Database"))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(t.text)
                                .child(harbor_client::paths::shorten(std::path::Path::new(
                                    &info.database,
                                ))),
                        )
                        .when_some(self.path_copy.clone(), |d, btn| d.child(btn)),
                )
                .when_some(catalog.database_size_bytes, |d, data| {
                    let h = |n: u64| crate::util::human(n as f64, "B");
                    d.child(meta(
                        t,
                        "Size",
                        match catalog.wal_size_bytes.unwrap_or(0) {
                            0 => h(data),
                            wal => format!("{} (WAL {})", h(data), h(wal)),
                        },
                    ))
                })
                .child(meta(
                    t,
                    "Uptime",
                    crate::util::human(info.uptime_ms as f64 / 1000., "s"),
                ))
                .child(meta(
                    t,
                    "Lifetime",
                    if conn.summoned { "started by this window".into() } else { "was already running".into() },
                ))
            }
            }
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

/// The Harbor version row, with an upgrade hint when the server runs behind the
/// installed binary: a red "harbor X available" for a local server (the
/// sidebar badge does the actual restart), a quiet yellow "behind harbor X" for
/// a remote one, which cannot be relaunched from here.
fn harbor_meta(
    t: Pal,
    version: &str,
    stale: bool,
    is_local: bool,
    installed: Option<&str>,
) -> impl IntoElement {
    div()
        .h_flex()
        .gap_2()
        .w_full()
        .items_center()
        .text_sm()
        .child(div().w_20().flex_none().text_color(t.muted).child("Harbor"))
        .child(div().flex_none().text_color(t.text).child(version.to_string()))
        .when(stale, |d| {
            let (color, label) = if is_local {
                (t.bad, format!("harbor {} available", installed.unwrap_or_default()))
            } else {
                (t.warn, format!("behind harbor {}", installed.unwrap_or_default()))
            };
            d.child(
                div()
                    .px_1()
                    .rounded_md()
                    .text_xs()
                    .text_color(color)
                    .border_1()
                    .border_color(color)
                    .child(label),
            )
        })
}
