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
    /// Table count, knowable only for live berths (a catalog fetch).
    pub(crate) tables: Option<usize>,
    /// Size on disk (data + WAL) — knowable for every berth.
    pub(crate) size: Option<u64>,
}

pub(crate) enum Phase {
    Idle,
    Connected {
        conn: Conn,
        info: wire::InfoResponse,
        catalog: harbor_client::Catalog,
        /// `PRAGMA database_size` figures for the identity card:
        /// (data bytes, wal bytes).
        db_size: Option<(u64, u64)>,
        /// Per-table row counts for the sidebar, (schema, table) ->
        /// estimated_size. Snapshot at connect, like the catalog.
        row_counts: std::collections::HashMap<(String, String), u64>,
    },
    Failed { name: String, message: String },
}

pub struct DuckTable {
    pub(crate) rows: Vec<RowVm>,
    pub(crate) phase: Phase,
    pub(crate) attempt: u64,
    pub(crate) selected_table: Option<(String, String)>,
    pub(crate) grid: Option<Entity<crate::grid::Grid>>,
    /// A connect in flight (berth name). The current phase keeps rendering
    /// until the outcome lands — a berth click never blanks the pane.
    pub(crate) connecting: Option<String>,
    /// The sidebar's table-name filter; Some = the field is open.
    pub(crate) table_filter: Option<Entity<gpui_component::input::InputState>>,
    /// The sidebar's database-name filter; Some = the field is open.
    pub(crate) berth_filter: Option<Entity<gpui_component::input::InputState>>,
    /// Fence for table selection: a first-page fetch that finishes after a
    /// newer click discards itself instead of swapping in a stale grid.
    select_seq: u64,
    /// Fence for the berth-list refresh: overlapping sweeps (a manual
    /// click racing the one connect fires) commit newest-wins instead of
    /// arbitrary order.
    refresh_seq: u64,
}

impl DuckTable {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            rows: Vec::new(),
            phase: Phase::Idle,
            attempt: 0,
            selected_table: None,
            grid: None,
            connecting: None,
            table_filter: None,
            berth_filter: None,
            select_seq: 0,
            refresh_seq: 0,
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
        let (conn, solo_schema) = match &self.phase {
            Phase::Connected { conn, catalog, .. } => {
                (conn.clone(), catalog.schemas().len() <= 1)
            }
            _ => return,
        };
        // "main.tests" earns its prefix only when there is another schema
        // to distinguish it from.
        let title =
            if solo_schema { clone_str(&name) } else { format!("{schema}.{name}") };
        self.selected_table = Some((clone_str(&schema), clone_str(&name)));
        self.select_seq += 1;
        let fence = self.select_seq;
        let page_size = crate::prefs::get(cx).page_size;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let (outcome, total, structure) = cx
                .background_executor()
                .spawn({
                    let conn = conn.clone();
                    let schema = clone_str(&schema);
                    let name = clone_str(&name);
                    async move {
                        let page = crate::grid::first_page(&conn, &schema, &name, page_size);
                        let (total, structure) = if page.is_ok() {
                            (
                                crate::grid::total_rows(&conn, &schema, &name),
                                crate::structure::table_structure(&conn, &schema, &name),
                            )
                        } else {
                            (None, None)
                        };
                        (page, total, structure)
                    }
                })
                .await;
            this.update_in(cx, |state, window, cx| {
                if state.select_seq != fence {
                    return;
                }
                if !matches!(state.phase, Phase::Connected { .. }) {
                    return;
                }
                // The Data/Structure choice is a browsing mode, not table
                // state: it carries over from the grid being replaced.
                let view = state
                    .grid
                    .as_ref()
                    .map(|g| g.read(cx).view())
                    .unwrap_or(crate::grid::ViewMode::Data);
                state.grid = Some(cx.new(|cx| {
                    let mut grid = crate::grid::Grid::new(
                        conn, &schema, &name, title, outcome, total, page_size, structure,
                        window, cx,
                    );
                    grid.set_view(view);
                    grid
                }));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// A fresh, focused filter input whose changes repaint the sidebar.
    fn new_filter(
        placeholder: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<gpui_component::input::InputState> {
        let input = cx.new(|cx| {
            gpui_component::input::InputState::new(window, cx).placeholder(placeholder)
        });
        cx.subscribe(&input, |_, _, _: &gpui_component::input::InputEvent, cx| {
            cx.notify();
        })
        .detach();
        input.update(cx, |state, cx| state.focus(window, cx));
        input
    }

    /// Open (focused) or close the sidebar's table filter.
    pub(crate) fn toggle_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.table_filter.take().is_none() {
            self.table_filter = Some(Self::new_filter("Filter tables", window, cx));
        }
        cx.notify();
    }

    /// Open (focused) or close the sidebar's database filter.
    pub(crate) fn toggle_berth_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.berth_filter.take().is_none() {
            self.berth_filter = Some(Self::new_filter("Filter databases", window, cx));
        }
        cx.notify();
    }

    /// Re-pull the catalog, row counts, and size for the live connection —
    /// the sidebar's snapshot goes stale when something else writes to the
    /// database. Fetch first; the old catalog stays until the new one
    /// lands, and a failed refresh changes nothing.
    pub(crate) fn refresh_catalog(&mut self, cx: &mut Context<Self>) {
        let conn = match &self.phase {
            Phase::Connected { conn, .. } => conn.clone(),
            _ => return,
        };
        let fence = self.attempt;
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    let catalog = harbor_client::catalog(&conn)?;
                    let db_size = crate::grid::database_size(&conn);
                    let counts = crate::grid::table_counts(&conn).unwrap_or_default();
                    Ok::<_, String>((catalog, db_size, counts))
                })
                .await;
            this.update(cx, |state, cx| {
                if state.attempt != fence {
                    return;
                }
                if let (
                    Phase::Connected { catalog, db_size, row_counts, .. },
                    Ok((new_catalog, new_size, new_counts)),
                ) = (&mut state.phase, outcome)
                {
                    *catalog = new_catalog;
                    *db_size = new_size;
                    *row_counts = new_counts;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        self.refresh_seq += 1;
        let fence = self.refresh_seq;
        // The connected berth's catalog is already in hand; its row must
        // not pay a second connect + catalog download just for a count.
        let connected: Option<(String, usize)> = match &self.phase {
            Phase::Connected { conn, catalog, .. } => Some((
                clone_str(&conn.name),
                catalog.schemas().iter().map(|s| catalog.tables_in(s).len()).sum(),
            )),
            _ => None,
        };
        cx.spawn(async move |this, cx| {
            let list = cx.background_executor().spawn(async move { fleet::list() }).await;
            // One task per berth so the probes run concurrently — a dead
            // berth's probe timeout no longer serializes behind the rest.
            let tasks: Vec<_> = list
                .into_iter()
                .map(|row| {
                    let known = connected.clone();
                    cx.background_executor().spawn(async move {
                        let live = row.transport.as_ref().map(fleet::probe);
                        // Table count needs the database open, so only
                        // live berths answer; size comes from the file
                        // on disk and works for every berth.
                        let tables = match &known {
                            Some((name, count)) if *name == row.name => Some(*count),
                            _ => (live == Some(true))
                                .then(|| {
                                    let conn = fleet::connect(&row.name).ok()?;
                                    let cat = harbor_client::catalog(&conn).ok()?;
                                    Some(
                                        cat.schemas()
                                            .iter()
                                            .map(|s| cat.tables_in(s).len())
                                            .sum(),
                                    )
                                })
                                .flatten(),
                        };
                        RowVm {
                            state: fleet::state_of(&row, live),
                            tables,
                            size: row.size_on_disk(),
                            name: row.name,
                        }
                    })
                })
                .collect();
            let mut rows = Vec::with_capacity(tasks.len());
            for task in tasks {
                rows.push(task.await);
            }
            this.update(cx, |state, cx| {
                if state.refresh_seq != fence {
                    return;
                }
                state.rows = rows;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Connect to a berth: the current content keeps rendering while the
    /// connect chain runs, and the whole pane swaps to the new berth in ONE
    /// frame when the outcome lands (same fetch-first rule as
    /// `select_table` — a click never flashes an intermediate state). The
    /// in-flight name shows on the sidebar row; the idle/failed cards show
    /// a connecting card since they hold nothing worth preserving.
    pub(crate) fn connect(&mut self, name: String, cx: &mut Context<Self>) {
        self.attempt += 1;
        let fence = self.attempt;
        self.connecting = Some(clone_str(&name));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let target = clone_str(&name);
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    let conn = fleet::connect(&target)?;
                    let info = fleet::info(&conn)?;
                    let catalog = harbor_client::catalog(&conn)?;
                    let db_size = crate::grid::database_size(&conn);
                    let row_counts = crate::grid::table_counts(&conn).unwrap_or_default();
                    Ok::<_, String>((conn, info, catalog, db_size, row_counts))
                })
                .await;
            this.update(cx, |state, cx| {
                if state.attempt != fence {
                    return;
                }
                state.connecting = None;
                state.selected_table = None;
                state.grid = None;
                state.select_seq += 1;
                state.phase = match outcome {
                    Ok((conn, info, catalog, db_size, row_counts)) => {
                        Phase::Connected { conn, info, catalog, db_size, row_counts }
                    }
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

    /// Abort the in-flight connect. The current phase never changed, so
    /// whatever was on screen simply stays (a cancelled connect is not a
    /// failed connect).
    pub(crate) fn cancel(&mut self, cx: &mut Context<Self>) {
        self.attempt += 1;
        self.connecting = None;
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
