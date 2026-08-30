//! DuckTable phase 1: the shell.
//!
//! A window with the berth sidebar (live states from harbor-client), a
//! connect flow whose cancel is synchronous and fenced, and a connected
//! pane showing the berth's identity. The grid, tabs and editor build on
//! this; the connection rules they inherit are already here: a late
//! completion checks its attempt fence and discards itself, and a
//! connected window pulses /keepalive so an idle-exit berth stays moored.

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::{button::*, *};
use harbor_client::{fleet, Conn, Level, State};
use std::time::Duration;

const BG_SIDEBAR: u32 = 0xF2F3F5;
const BG_SURFACE: u32 = 0xFFFFFF;
const TEXT: u32 = 0x1D1D1F;
const MUTED: u32 = 0x71717A;
const ACCENT: u32 = 0x007AFF;
const BORDER: u32 = 0xD9DBE1;
const GOOD: u32 = 0x28A745;
const WARN: u32 = 0xB58A00;
const BAD: u32 = 0xD93025;

#[derive(Clone)]
struct RowVm {
    name: String,
    state: State,
    summonable: bool,
}

enum Phase {
    Idle,
    Connecting { name: String },
    Connected { conn: Conn, info: wire::InfoResponse },
    Failed { name: String, message: String },
}

pub struct DuckTable {
    rows: Vec<RowVm>,
    phase: Phase,
    attempt: u64,
}

impl DuckTable {
    fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self { rows: Vec::new(), phase: Phase::Idle, attempt: 0 };
        this.refresh(cx);
        this
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let rows = cx
                .background_executor()
                .spawn(async move {
                    fleet::list()
                        .into_iter()
                        .map(|row| {
                            let live = row.transport.as_ref().map(fleet::probe);
                            RowVm {
                                state: fleet::state_of(&row, live),
                                summonable: row.summonable(),
                                name: row.name,
                            }
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            this.update(cx, |state, cx| {
                state.rows = rows;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn connect(&mut self, name: String, cx: &mut Context<Self>) {
        self.attempt += 1;
        let fence = self.attempt;
        self.phase = Phase::Connecting { name: clone_str(&name) };
        cx.notify();
        cx.spawn(async move |this, cx| {
            let target = clone_str(&name);
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    let conn = fleet::connect(&target)?;
                    let info = fleet::info(&conn)?;
                    Ok::<_, String>((conn, info))
                })
                .await;
            this.update(cx, |state, cx| {
                if state.attempt != fence {
                    return;
                }
                state.phase = match outcome {
                    Ok((conn, info)) => Phase::Connected { conn, info },
                    Err(message) => Phase::Failed { name: clone_str(&name), message },
                };
                if let Phase::Connected { .. } = state.phase {
                    state.keepalive(fence, cx);
                }
                state.refresh(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        self.attempt += 1;
        self.phase = Phase::Idle;
        cx.notify();
    }

    fn keepalive(&self, fence: u64, cx: &mut Context<Self>) {
        let conn = match &self.phase {
            Phase::Connected { conn, .. } => conn.clone(),
            _ => return,
        };
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(30)).await;
                let still = this
                    .read_with(cx, |state, _| state.attempt == fence)
                    .unwrap_or(false);
                if !still {
                    return;
                }
                let conn = conn.clone();
                cx.background_executor().spawn(async move { fleet::keepalive(&conn) }).await;
            }
        })
        .detach();
    }

    fn dot(level: Level) -> Div {
        let color = match level {
            Level::Good => rgb(GOOD),
            Level::Warn => rgb(WARN),
            Level::Bad => rgb(BAD),
            Level::Idle => rgb(MUTED),
        };
        div().size_2().rounded_full().bg(color)
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = match &self.phase {
            Phase::Connected { conn, .. } => Some(clone_str(&conn.name)),
            Phase::Connecting { name } => Some(clone_str(name)),
            _ => None,
        };
        div()
            .w_56()
            .flex_none()
            .h_full()
            .bg(rgb(BG_SIDEBAR))
            .border_r_1()
            .border_color(rgb(BORDER))
            .p_2()
            .v_flex()
            .gap_px()
            .child(
                div()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .font_weight(FontWeight::BOLD)
                    .text_color(rgb(MUTED))
                    .child("BERTHS"),
            )
            .children(self.rows.iter().map(|row| {
                let name = clone_str(&row.name);
                let selected = active.as_deref() == Some(row.name.as_str());
                div()
                    .id(SharedString::from(clone_str(&row.name)))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .h_flex()
                    .gap_2()
                    .items_center()
                    .cursor_pointer()
                    .when(selected, |d| d.bg(rgb(0xD6E6FB)))
                    .hover(|d| d.bg(rgb(0xE4EDF8)))
                    .child(Self::dot(row.state.level()))
                    .child(div().flex_1().text_sm().text_color(rgb(TEXT)).child(clone_str(&row.name)))
                    .when(row.summonable, |d| {
                        d.child(div().text_xs().text_color(rgb(MUTED)).child("spawn"))
                    })
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.connect(clone_str(&name), cx);
                    }))
            }))
            .child(
                div()
                    .id("refresh")
                    .mt_2()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .text_color(rgb(ACCENT))
                    .cursor_pointer()
                    .child("refresh")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.refresh(cx))),
            )
    }

    fn content(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
            Phase::Connected { conn, info } => div()
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

fn clone_str(s: &str) -> String {
    s.to_string()
}

impl Render for DuckTable {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .h_flex()
            .font_family(".SystemUIFont")
            .child(self.sidebar(cx))
            .child(self.content(cx))
    }
}

fn main() {
    let app = Application::new();

    app.run(move |cx| {
        gpui_component::init(cx);

        cx.spawn(async move |cx| {
            cx.open_window(
                WindowOptions {
                    window_min_size: Some(size(px(720.), px(420.))),
                    ..Default::default()
                },
                |window, cx| {
                    let view = cx.new(DuckTable::new);
                    cx.new(|cx| Root::new(view, window, cx))
                },
            )?;

            Ok::<_, anyhow::Error>(())
        })
        .detach();
    });
}
