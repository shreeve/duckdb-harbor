//! The root entity: connection state and its lifecycle rules.
//!
//! Every mutation of phase or attempt lives here. The rendering files
//! (`sidebar.rs`, `content.rs`) read this state and call back into these
//! methods; they never mutate it themselves. The attempt counter is the
//! fence: a late completion compares its fence and discards itself.

use crate::util::clone_str;
use gpui::*;
use harbor_client::{fleet, Conn, State};

pub(crate) struct RowVm {
    pub(crate) name: String,
    pub(crate) state: State,
    /// On your list (a `[connection.*]` in config.toml) — what the menu shows
    /// Attach vs Detach from.
    pub(crate) attached: bool,
    /// A login item exists — the Autostart menu item's checkmark.
    pub(crate) autostart: bool,
    /// The database file, when this is a local berth — what the lifecycle
    /// menu items target.
    pub(crate) path: Option<std::path::PathBuf>,
    /// Table count, knowable only for live berths (a catalog fetch).
    pub(crate) tables: Option<usize>,
    /// Size on disk (data + WAL) — knowable for every berth.
    pub(crate) size: Option<u64>,
    /// the survey's human-readable note for an unusual row,
    /// surfaced as the row's tooltip.
    pub(crate) note: Option<String>,
    /// The harbor version a running server reports (`None` when stopped).
    pub(crate) version: Option<String>,
    /// Whether a running server self-retires with its last client — the mode
    /// an upgrade restart preserves.
    pub(crate) ephemeral: bool,
}

impl RowVm {
    /// A local, running server older than the installed binary: actionable,
    /// because it can be restarted onto the new version. Remote rows (no path)
    /// never qualify — you cannot relaunch someone else's server from here.
    pub(crate) fn upgradable(&self, installed: &str) -> bool {
        self.path.is_some()
            && self.state.is_live()
            && self
                .version
                .as_deref()
                .is_some_and(|v| harbor_client::fleet::version_older(v, installed))
    }
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
    /// The sidebar/content divider (UI.md: divider positions persist —
    /// the width saves at the end of each drag).
    pub(crate) sidebar_resize: Entity<gpui_component::resizable::ResizableState>,
    /// Berths with a Stop in flight: the row keeps its slot but swaps its
    /// dot for a spinner and stops taking clicks until the shutdown lands.
    pub(crate) stopping: std::collections::HashSet<String>,
    /// Berths mid-departure: the shutdown returned and the survey no
    /// longer reports them, but the row lingers one fade before it's
    /// dropped. `refresh` re-splices these so the survey's removal can't
    /// yank a row out from under its own fade-out.
    pub(crate) leaving: std::collections::HashSet<String>,
    /// The connected berth's info-card copy tile for the database path —
    /// the same self-confirming widget the DDL block uses. Rebuilt on each
    /// connect (it holds the path it copies), None when not connected.
    pub(crate) path_copy: Option<Entity<crate::copy_button::CopyButton>>,
    /// The version of the `harbor` binary this app spawns — the yardstick a
    /// row's reported version is judged outdated against. Re-probed on each
    /// refresh, so installing a newer binary lights up the upgrade badge
    /// without a restart of the app. `None` until the first probe answers.
    pub(crate) installed_version: Option<String>,
}

impl DuckTable {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        let sidebar_resize =
            cx.new(|_| gpui_component::resizable::ResizableState::default());
        cx.subscribe(
            &sidebar_resize,
            |_, state, _: &gpui_component::resizable::ResizablePanelEvent, cx| {
                if let Some(width) = state.read(cx).sizes().first().copied() {
                    crate::prefs::save(cx, |p| {
                        p.sidebar_width = f32::from(width)
                            .clamp(crate::prefs::SIDEBAR_MIN, crate::prefs::SIDEBAR_MAX);
                    });
                }
            },
        )
        .detach();
        // Escape in a sidebar filter, macOS filter-field style: text
        // present -> first press clears it (retype without losing the
        // box); empty -> the press dismisses the filter. One
        // interceptor outlives every toggled filter instance.
        let weak = cx.entity().downgrade();
        cx.intercept_keystrokes(move |ev, window, cx| {
            if ev.keystroke.key != "escape" {
                return;
            }
            let Some(app) = weak.upgrade() else { return };
            let filters =
                [app.read(cx).table_filter.clone(), app.read(cx).berth_filter.clone()];
            for input in filters.into_iter().flatten() {
                if !input.read(cx).focus_handle(cx).is_focused(window) {
                    continue;
                }
                if input.read(cx).value().is_empty() {
                    app.update(cx, |app, cx| {
                        if app.table_filter.as_ref() == Some(&input) {
                            app.toggle_table_filter(window, cx);
                        } else {
                            app.toggle_berth_filter(window, cx);
                        }
                    });
                } else {
                    input.update(cx, |state, cx| state.set_value("", window, cx));
                }
                cx.stop_propagation();
                return;
            }
        })
        .detach();
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
            sidebar_resize,
            stopping: std::collections::HashSet::new(),
            leaving: std::collections::HashSet::new(),
            path_copy: None,
            installed_version: None,
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
                async move { crate::sql::first_page(&conn, &schema, &name, rowid, page_size) }
            });
            let total_task = cx.background_executor().spawn({
                let (conn, schema, name) = (conn.clone(), clone_str(&schema), clone_str(&name));
                async move { crate::sql::total_rows(&conn, &schema, &name) }
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
                let source = crate::sql::source(&schema, &name);
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
                grid.update(cx, |g, cx| {
                    g.query_view = state.query.clone();
                    g.query_obs = g
                        .query_view
                        .as_ref()
                        .map(|q| cx.observe(q, |_, _, cx| cx.notify()));
                });
                // A fresh grid hears the keyboard at once: landing on a
                // table and pressing ↓ must navigate, not vanish into
                // the sidebar. Data only — Query keeps its editor.
                if crate::prefs::get(cx).view == crate::prefs::ViewMode::Data {
                    grid.update(cx, |g, cx| g.request_focus(cx));
                }
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
    /// ⌥←/⌥→: the previous/next table, walking the sidebar's own order
    /// and filter (sidebar.rs visible_tables) — with rollover, so the
    /// tables read as a ring you can circle rather than a hall that
    /// dead-ends (Steve's ruling). With nothing selected yet, either
    /// arrow lands on the nearest end.
    pub(crate) fn step_table(
        &mut self,
        delta: i32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let list = self.visible_tables(cx);
        if list.is_empty() {
            return;
        }
        let ix = self
            .selected_table
            .as_ref()
            .and_then(|sel| list.iter().position(|k| k == sel));
        let next = match ix {
            Some(i) => (i as i32 + delta).rem_euclid(list.len() as i32) as usize,
            None => {
                if delta >= 0 {
                    0
                } else {
                    list.len() - 1
                }
            }
        };
        if Some(&list[next]) == self.selected_table.as_ref() {
            return; // a one-table ring goes nowhere
        }
        let (schema, name) = list[next].clone();
        self.select_table(schema, name, window, cx);
    }

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
                    // The fetch failed — most often because the server departed
                    // while we held it. Reconcile against the survey instead of
                    // surfacing a raw OS error: refresh drops a dead connection
                    // (with a way back) or, if the server is fine, re-surveys.
                    Err(_) => state.refresh(cx),
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
            // Re-probed each sweep so a freshly installed binary lights the
            // upgrade badge without relaunching the app.
            let installed_version = cx
                .background_executor()
                .spawn(async move { fleet::installed_harbor_version() })
                .await;
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
                            attached: row.attached,
                            autostart: row.autostart,
                            path: row.path,
                            tables,
                            size: row.size,
                            note: row.note,
                            name: row.name,
                            version: row.version,
                            ephemeral: row.ephemeral,
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
                // Keep departing rows on screen through their fade: the
                // survey has already forgotten a stopped berth, but its
                // row must linger until the fade timer drops it. Re-splice
                // each leaving ghost at (near) its old index so nothing
                // below it jumps while it dims.
                if !state.leaving.is_empty() {
                    let mut old = std::mem::take(&mut state.rows);
                    let mut carried: Vec<(usize, RowVm)> = Vec::new();
                    for (i, r) in old.drain(..).enumerate() {
                        if state.leaving.contains(&r.name)
                            && !rows.iter().any(|n| n.name == r.name)
                        {
                            carried.push((i, r));
                        }
                    }
                    for (i, r) in carried {
                        let at = i.min(rows.len());
                        rows.insert(at, r);
                    }
                }
                state.rows = rows;
                state.warning = warning;
                state.installed_version = installed_version;
                // Reconcile the connection against the survey's truth: if we
                // still think we're connected to a berth the survey no longer
                // shows running, its server exited out from under us. Drop it
                // cleanly and point the way back, rather than leaving a dead
                // connection to fail the next catalog or query with an OS error.
                let connected = match &state.phase {
                    Phase::Connected { info, .. } => Some(clone_str(&info.name)),
                    _ => None,
                };
                if let Some(name) = connected
                    && !state.rows.iter().any(|r| r.name == name && r.state.is_live())
                {
                    state.drop_connection(cx);
                    state.warning = Some(format!("{name} stopped — click it to reconnect"));
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The local, running servers older than the installed binary — the ones a
    /// one-click upgrade would restart. Empty when the version is unknown, so a
    /// failed probe never nags.
    pub(crate) fn outdated(&self) -> Vec<&RowVm> {
        let Some(installed) = self.installed_version.as_deref() else { return Vec::new() };
        self.rows.iter().filter(|r| r.upgradable(installed)).collect()
    }

    /// How many local servers are outdated — the upgrade badge's number,
    /// counted without allocating on every sidebar paint.
    pub(crate) fn outdated_count(&self) -> usize {
        let Some(installed) = self.installed_version.as_deref() else { return 0 };
        self.rows.iter().filter(|r| r.upgradable(installed)).count()
    }

    /// Upgrade every outdated local server: restart each onto the installed
    /// binary in the mode it was running, then refresh so the badge clears as
    /// they come back current. Runs on a background thread; the first failure
    /// is surfaced, the rest still attempted.
    pub(crate) fn upgrade_outdated(&mut self, cx: &mut Context<Self>) {
        let targets: Vec<(std::path::PathBuf, bool)> = self
            .outdated()
            .iter()
            .filter_map(|r| r.path.clone().map(|p| (p, r.ephemeral)))
            .collect();
        if targets.is_empty() {
            return;
        }
        self.fleet_then_refresh(
            move || {
                let mut first_err = None;
                for (path, ephemeral) in targets {
                    if let Err(e) = fleet::restart(&path, ephemeral) {
                        first_err.get_or_insert(e);
                    }
                }
                first_err.map_or(Ok(()), Err)
            },
            cx,
        );
    }

    /// The upgrade badge's action: name the count, confirm once, and on yes
    /// restart every outdated local server onto the installed binary.
    pub(crate) fn prompt_upgrade(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.outdated().len();
        if n == 0 {
            return;
        }
        let installed = self.installed_version.clone().unwrap_or_default();
        let noun = if n == 1 { "database".to_string() } else { format!("{n} databases") };
        let title = format!("Upgrade {noun}");
        let body = format!(
            "Restart {noun} onto harbor {installed}. Each server stops and comes \
             back in the same mode; any connected clients reconnect."
        );
        let answer =
            window.prompt(PromptLevel::Info, &title, Some(&body), &["Upgrade", "Cancel"], cx);
        cx.spawn(async move |this, cx| {
            if answer.await == Ok(0) {
                this.update(cx, |state, cx| state.upgrade_outdated(cx)).ok();
            }
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
        let target = clone_str(&name);
        self.dial(
            name,
            move || {
                let conn = fleet::connect(&target)?;
                let info = fleet::info(&conn)?;
                let catalog = harbor_client::catalog(&conn)?;
                Ok((conn, info, catalog))
            },
            cx,
        );
    }

    /// The shared spine of connect / open_path: show `shown` as the connecting
    /// label under a fresh fence, run `dial` (raise-or-join the server and read
    /// its catalog) on a background thread, then swap the pane to the outcome
    /// in one frame. A stale fence discards itself, so a slow attempt never
    /// clobbers a newer one; current content keeps rendering until it lands.
    fn dial<F>(&mut self, shown: String, dial: F, cx: &mut Context<Self>)
    where
        F: FnOnce() -> Result<(Conn, wire::InfoResponse, harbor_client::Catalog), String>
            + Send
            + 'static,
    {
        self.attempt += 1;
        let fence = self.attempt;
        self.connecting = Some(clone_str(&shown));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let outcome = cx.background_executor().spawn(async move { dial() }).await;
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
                    Err(message) => Phase::Failed { name: clone_str(&shown), message },
                };
                state.sync_path_copy(cx);
                state.refresh(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Rebuild the info-card path copy tile from the current phase: a fresh
    /// widget carrying the connected berth's (shortened) path, or None when
    /// not connected. Called at every phase change, never from the fleet
    /// refresh (that leaves the connected berth in place).
    fn sync_path_copy(&mut self, cx: &mut Context<Self>) {
        self.path_copy = match &self.phase {
            Phase::Connected { info, .. } => {
                let p = harbor_client::paths::shorten(std::path::Path::new(&info.database));
                Some(cx.new(|_| crate::copy_button::CopyButton::new("Copy path", p)))
            }
            _ => None,
        };
    }

    /// File→Open and drag-drop land here: connect to a database FILE the
    /// picker or the drop named. No config entry needed — the path is the
    /// target — and the flow is `connect`'s exactly: current content keeps
    /// rendering, the pane swaps in one frame when the outcome lands, and
    /// the refresh that follows shows the server under its own /info name.
    /// This is the trunk the open-anything dispatcher (CSV, Parquet,
    /// Sheets URLs…) grows from later; today it speaks .duckdb.
    pub(crate) fn open_path(&mut self, path: std::path::PathBuf, cx: &mut Context<Self>) {
        let shown = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        self.dial(
            shown,
            move || {
                let conn = fleet::connect_path(&path)?;
                let info = fleet::info(&conn)?;
                let catalog = harbor_client::catalog(&conn)?;
                Ok((conn, info, catalog))
            },
            cx,
        );
    }

    /// Clear the connected world back to Idle, forgetting its table, grid,
    /// query, and staged edits. Shared by Stop and by refresh's reconciliation
    /// when the server exits out from under us — either way there is nothing
    /// left to show, and a lingering dead connection would only fail the next
    /// catalog or query with a raw OS error.
    fn drop_connection(&mut self, cx: &mut Context<Self>) {
        self.phase = Phase::Idle;
        self.selected_table = None;
        self.grid = None;
        self.query = None;
        self.staged.clear();
        self.select_seq += 1;
        self.sync_path_copy(cx);
    }

    /// Stop a berth's server — the close half of open. Right-click → Stop
    /// lands here: POST /shutdown to the named server, then refresh so its
    /// row goes from green to stopped (or leaves, if it was ephemeral). If
    /// the berth we're viewing is the one stopped, the view returns to Idle
    /// — a stopped server has nothing to show.
    pub(crate) fn stop_berth(&mut self, name: String, cx: &mut Context<Self>) {
        // Idempotent: a second Stop while one is already in flight (or the
        // row is already fading out) is a no-op.
        if self.stopping.contains(&name) || self.leaving.contains(&name) {
            return;
        }
        let connected_here = matches!(
            &self.phase,
            Phase::Connected { info, .. } if info.name == name
        );
        // The row keeps its slot and spins while the shutdown runs.
        self.stopping.insert(clone_str(&name));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let target = clone_str(&name);
            let outcome =
                cx.background_executor().spawn(async move { fleet::stop(&target) }).await;
            let stopped = this
                .update(cx, |state, cx| {
                    state.stopping.remove(&name);
                    match outcome {
                        Err(message) => {
                            // The berth is still alive — no fade, no reset.
                            state.warning = Some(message);
                            state.refresh(cx);
                            cx.notify();
                            false
                        }
                        Ok(()) => {
                            // It departed: hold the row for one fade, then
                            // let refresh's survey drop it for real.
                            state.leaving.insert(clone_str(&name));
                            if connected_here {
                                // The world we were showing just departed.
                                state.drop_connection(cx);
                            }
                            state.refresh(cx);
                            cx.notify();
                            true
                        }
                    }
                })
                .unwrap_or(false);
            if !stopped {
                return;
            }
            // Fade-out window (must outlast FADE_MS in the sidebar), then
            // drop the ghost so the gap closes.
            cx.background_executor().timer(std::time::Duration::from_millis(260)).await;
            this.update(cx, |state, cx| {
                state.leaving.remove(&name);
                state.rows.retain(|r| r.name != name);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Run a one-shot fleet operation on a background thread, then refresh the
    /// list; any error becomes the warning banner. The shared body behind
    /// Start / Attach / Detach / Auto-start — each differs only in the call it
    /// makes, none touches the phase or fences (that is `dial`'s job).
    fn fleet_then_refresh(
        &self,
        op: impl FnOnce() -> Result<(), String> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let outcome = cx.background_executor().spawn(async move { op() }).await;
            this.update(cx, |state, cx| {
                if let Err(message) = outcome {
                    state.warning = Some(message);
                }
                state.refresh(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Start a persistent server for a stopped berth, then refresh the list.
    pub(crate) fn start_berth(&mut self, path: std::path::PathBuf, cx: &mut Context<Self>) {
        self.fleet_then_refresh(move || fleet::start(&path), cx);
    }

    /// Add a berth to the list (config.toml), then refresh so Attach flips to
    /// Detach.
    pub(crate) fn attach_berth(&mut self, path: std::path::PathBuf, cx: &mut Context<Self>) {
        self.fleet_then_refresh(move || fleet::attach(&path), cx);
    }

    /// Remove a berth from the list, then refresh.
    pub(crate) fn detach_berth(&mut self, path: std::path::PathBuf, cx: &mut Context<Self>) {
        self.fleet_then_refresh(move || fleet::detach(&path), cx);
    }

    /// Arm or disarm the login item for a berth, then refresh so the checkmark
    /// flips. Arming never starts the database — running stays Start/Stop's job.
    pub(crate) fn toggle_autostart(
        &mut self,
        path: std::path::PathBuf,
        on: bool,
        cx: &mut Context<Self>,
    ) {
        self.fleet_then_refresh(move || fleet::set_autostart(&path, on), cx);
    }

    /// Abort the in-flight connect. The current phase never changed, so
    /// whatever was on screen simply stays (a cancelled connect is not a
    /// failed connect).
    pub(crate) fn cancel(&mut self, cx: &mut Context<Self>) {
        self.attempt += 1;
        self.connecting = None;
        cx.notify();
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
