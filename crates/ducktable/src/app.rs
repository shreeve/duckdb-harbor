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

pub(crate) struct RowVm {
    pub(crate) name: String,
    pub(crate) state: State,
    /// Table count, knowable only for live berths (a catalog fetch).
    pub(crate) tables: Option<usize>,
    /// Size on disk (data + WAL) — knowable for every berth.
    pub(crate) size: Option<u64>,
    /// reconcile's human-readable fix for an unhealthy row
    /// ("… harbor forget x"), surfaced as the row's tooltip.
    pub(crate) note: Option<String>,
}

pub(crate) enum Phase {
    Idle,
    Connected {
        conn: Conn,
        info: wire::InfoResponse,
        /// The one snapshot everything schema-shaped renders from: tables,
        /// columns, DDL, row estimates, and the file's size on disk all
        /// arrive in this single document (harbor 0.18+).
        catalog: harbor_client::Catalog,
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
    /// The sidebar's out-loud line: a refused config, or a catalog
    /// refresh that failed — a GUI has no stderr, and both would
    /// otherwise fail silently (an unexplained empty list reads as
    /// "harbor is broken"; a dead refresh click reads as "it worked").
    /// The next fleet refresh rewrites it from the config's truth.
    pub(crate) warning: Option<String>,
    /// The berth's one Query scratchpad (docs/QUERY.md law 1): owned
    /// here so table switches never touch it; rebuilt per berth.
    pub(crate) query: Option<Entity<crate::query::QueryView>>,
    /// Staged edits parked while their table is off-screen (Law 4 in
    /// docs/EDITING.md: staged changes belong to the table, not the
    /// view). Keyed by source; handed back when the table's grid is
    /// rebuilt, cleared on disconnect (a new berth is a new world).
    staged: std::collections::HashMap<String, crate::edits::Edits>,
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
            warning: None,
            query: None,
            staged: std::collections::HashMap::new(),
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
        let (conn, solo_schema, structure) = match &self.phase {
            Phase::Connected { conn, catalog, .. } => (
                conn.clone(),
                catalog.schemas().len() <= 1,
                catalog
                    .tables
                    .iter()
                    .find(|t| t.schema == schema && t.name == name)
                    .map(crate::structure::table_structure),
            ),
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
            // Two independent queries (each on its own connection), so they
            // run concurrently — click latency is the slower one, not the
            // sum. The structure came free out of the catalog snapshot. A
            // failed page drops the count unawaited.
            // Keyless tables fetch DuckDB's implicit rowid as their
            // editing identity — the same predicate Grid::new applies.
            let rowid = structure
                .as_ref()
                .is_some_and(|s| !s.cols.iter().any(|c| c.pk));
            let page_task = cx.background_executor().spawn({
                let (conn, schema, name) = (conn.clone(), clone_str(&schema), clone_str(&name));
                async move { crate::queries::first_page(&conn, &schema, &name, rowid, page_size) }
            });
            let total_task = cx.background_executor().spawn({
                let (conn, schema, name) = (conn.clone(), clone_str(&schema), clone_str(&name));
                async move { crate::queries::total_rows(&conn, &schema, &name) }
            });
            let outcome = page_task.await;
            let total = if outcome.is_ok() { total_task.await } else { None };
            this.update_in(cx, |state, window, cx| {
                if state.select_seq != fence {
                    return;
                }
                if !matches!(state.phase, Phase::Connected { .. }) {
                    return;
                }
                // Staged edits outlive the grid that collected them (Law
                // 4): park the outgoing table's, keyed by source, before
                // the swap discards its view.
                if let Some(old) = state.grid.take() {
                    if let Some(edits) = old.update(cx, |g, _| g.take_edits()) {
                        state.staged.insert(edits.source().to_string(), edits);
                    }
                }
                // The Data/Structure choice is a browsing mode, not table
                // state (prefs.view): it survives this table switch.
                let grid = cx.new(|cx| {
                    crate::grid::Grid::new(
                        conn, &schema, &name, title, outcome, total, page_size, structure,
                        window, cx,
                    )
                });
                // And returning to a table hands its parked edits back.
                let source = crate::queries::source(&schema, &name);
                if let Some(stash) = state.staged.remove(&source) {
                    grid.update(cx, |g, cx| g.adopt_edits(stash, cx));
                }
                // The berth's scratchpad rides along: created once per
                // berth, injected into every grid it outlives.
                let berth = match &state.phase {
                    Phase::Connected { info, .. } => clone_str(&info.name),
                    _ => String::new(),
                };
                if !state.query.as_ref().is_some_and(|q| q.read(cx).is_for(&berth)) {
                    let qconn = grid.read(cx).conn.clone();
                    state.query = Some(cx.new(|cx| {
                        crate::query::QueryView::new(qconn, &berth, window, cx)
                    }));
                }
                grid.update(cx, |g, _| {
                    g.query_view = state.query.clone().map(|q| q.into());
                });
                state.grid = Some(grid);
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
    pub(crate) fn toggle_table_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    /// Re-pull the catalog for the live connection — the sidebar's
    /// snapshot goes stale when something else writes to the database.
    /// Fetch first; the old catalog stays until the new one lands, and a
    /// failed refresh changes nothing.
    pub(crate) fn refresh_catalog(&mut self, cx: &mut Context<Self>) {
        let conn = match &self.phase {
            Phase::Connected { conn, .. } => conn.clone(),
            _ => return,
        };
        let fence = self.attempt;
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move { harbor_client::catalog(&conn) })
                .await;
            this.update(cx, |state, cx| {
                if state.attempt != fence {
                    return;
                }
                match outcome {
                    Ok(new_catalog) => {
                        if let Phase::Connected { catalog, .. } = &mut state.phase {
                            *catalog = new_catalog;
                        }
                    }
                    // A dead refresh click must not read as "nothing
                    // changed, so nothing was wrong".
                    Err(e) => state.warning = Some(format!("catalog refresh failed: {e}")),
                }
                cx.notify();
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
            Phase::Connected { conn, catalog, .. } => {
                Some((clone_str(&conn.name), catalog.tables.len()))
            }
            _ => None,
        };
        cx.spawn(async move |this, cx| {
            // survey() answers liveness from the lock files (flock is
            // proof of life), so this makes no probe in the common case
            // — and sees rows the old sidecar-only scan could not
            // (stale locks, running-but-unregistered berths).
            let fleet = cx.background_executor().spawn(async move { fleet::survey() }).await;
            let warning = fleet.warning;
            // One task per berth for the catalog fetches (table counts
            // need the database open; only live berths answer).
            let tasks: Vec<_> = fleet
                .rows
                .into_iter()
                .map(|row| {
                    let known = connected.clone();
                    cx.background_executor().spawn(async move {
                        let tables = match &known {
                            Some((name, count)) if *name == row.name => Some(*count),
                            _ => row
                                .state
                                .is_live()
                                .then(|| {
                                    let conn = fleet::connect(&row.name).ok()?;
                                    // Lite: this sweep only counts tables,
                                    // so it never pays for columns or DDL.
                                    let cat = harbor_client::catalog_lite(&conn).ok()?;
                                    Some(cat.tables.len())
                                })
                                .flatten(),
                        };
                        RowVm {
                            state: row.state,
                            tables,
                            size: row.size,
                            note: row.note,
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
                state.warning = warning;
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
                    Ok::<_, String>((conn, info, catalog))
                })
                .await;
            this.update(cx, |state, cx| {
                if state.attempt != fence {
                    return;
                }
                state.connecting = None;
                state.selected_table = None;
                state.grid = None;
                state.query = None;
                state.staged.clear();
                state.select_seq += 1;
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

impl DuckTable {
    /// The carousel landed on Query: hand focus to the editor (the
    /// symmetry of landing on Data focusing the grid).
    pub(crate) fn focus_query(&self, cx: &mut gpui::App) {
        if let Some(q) = &self.query {
            q.update(cx, |q, cx| q.request_focus(cx));
        }
    }
}
