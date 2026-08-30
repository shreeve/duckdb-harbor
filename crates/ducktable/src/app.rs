//! The root entity: connection state and its lifecycle rules.
//!
//! Every mutation of phase or attempt lives here. The rendering files
//! (`sidebar.rs`, `content.rs`) read this state and call back into these
//! methods; they never mutate it themselves. The attempt counter is the
//! fence: a late completion compares its fence and discards itself.

use crate::util::clone_str;
use gpui::*;
use harbor_client::{fleet, Conn, State};
use std::time::Duration;

#[derive(Clone)]
pub(crate) struct RowVm {
    pub(crate) name: String,
    pub(crate) state: State,
    pub(crate) summonable: bool,
}

pub(crate) enum Phase {
    Idle,
    Connecting { name: String },
    Connected { conn: Conn, info: wire::InfoResponse, catalog: harbor_client::Catalog },
    Failed { name: String, message: String },
}

pub struct DuckTable {
    pub(crate) rows: Vec<RowVm>,
    pub(crate) phase: Phase,
    pub(crate) attempt: u64,
    pub(crate) selected_table: Option<(String, String)>,
    pub(crate) grid: Option<Entity<crate::grid::Grid>>,
    /// Fence for table selection: a first-page fetch that finishes after a
    /// newer click discards itself instead of swapping in a stale grid.
    select_seq: u64,
}

impl DuckTable {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            rows: Vec::new(),
            phase: Phase::Idle,
            attempt: 0,
            selected_table: None,
            grid: None,
            select_seq: 0,
        };
        this.refresh(cx);
        this
    }

    /// Select a table: highlight immediately, fetch its first page in the
    /// background, and swap the grid in ONE frame once the data is ready.
    /// The old grid stays on screen until then — a click never shows a
    /// skeleton or columns popping in (DESIGN.md: fetch first, commit over
    /// the old value). The fence discards a stale fetch when the user has
    /// already clicked elsewhere.
    pub(crate) fn select_table(
        &mut self,
        schema: String,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let conn = match &self.phase {
            Phase::Connected { conn, .. } => conn.clone(),
            _ => return,
        };
        self.selected_table = Some((clone_str(&schema), clone_str(&name)));
        self.select_seq += 1;
        let fence = self.select_seq;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn({
                    let conn = conn.clone();
                    let schema = clone_str(&schema);
                    let name = clone_str(&name);
                    async move { crate::grid::first_page(&conn, &schema, &name) }
                })
                .await;
            this.update_in(cx, |state, window, cx| {
                if state.select_seq != fence {
                    return;
                }
                if !matches!(state.phase, Phase::Connected { .. }) {
                    return;
                }
                state.grid = Some(cx.new(|cx| {
                    crate::grid::Grid::new(conn, &schema, &name, outcome, window, cx)
                }));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
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

    pub(crate) fn connect(&mut self, name: String, cx: &mut Context<Self>) {
        self.attempt += 1;
        let fence = self.attempt;
        self.phase = Phase::Connecting { name: clone_str(&name) };
        self.selected_table = None;
        self.grid = None;
        self.select_seq += 1;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let target = clone_str(&name);
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    let conn = fleet::connect(&target)?;
                    let info = fleet::info(&conn)?;
                    let catalog = harbor_client::catalog(&conn)?;
                    Ok::<_, String>((conn, info, catalog))
                })
                .await;
            this.update(cx, |state, cx| {
                if state.attempt != fence {
                    return;
                }
                state.phase = match outcome {
                    Ok((conn, info, catalog)) => Phase::Connected { conn, info, catalog },
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

    pub(crate) fn cancel(&mut self, cx: &mut Context<Self>) {
        self.attempt += 1;
        self.phase = Phase::Idle;
        self.selected_table = None;
        self.grid = None;
        self.select_seq += 1;
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
}
