//! The root entity: connection state and its lifecycle rules.
//!
//! Every mutation of phase or attempt lives here. The rendering files
//! (`sidebar.rs`, `content.rs`) read this state and call back into these
//! methods; they never mutate it themselves. The attempt counter is the
//! fence: a late completion compares its fence and discards itself.

use crate::util::clone_str;
use gpui_kit::*;
use harbor_client::{fleet, Conn, State};

fn catalog_refresh_is_current(
    current_attempt: u64,
    current_refresh: u64,
    fenced_attempt: u64,
    fenced_refresh: u64,
) -> bool {
    current_attempt == fenced_attempt && current_refresh == fenced_refresh
}

/// A child surface finished work that may have changed any table. The root
/// owns the catalog snapshot, so grids and query views request a refresh
/// instead of trying to update sidebar counts themselves.
pub(crate) struct CatalogRefreshRequested;

/// A grid's commit has landed or failed: its staged set is now either empty
/// or kept, and a table switch that waited on it can run.
pub(crate) struct CommitSettled;

/// The row the sidebar lights: the database being dialed while a connect is
/// in flight, the connected one otherwise.
fn active_key<'a>(connecting: Option<&'a DbKey>, connected: Option<&'a DbKey>) -> Option<&'a DbKey> {
    connecting.or(connected)
}

/// What quitting would lose or leave unreported. Law 2 (docs/EDITING.md)
/// makes staged changes the only place work lives before ⌘S, so a quit
/// asks before it discards them.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct QuitRisks {
    /// Staged changes not yet sent, over every table that holds some: the
    /// one on screen and those parked by a table switch. The changes of a
    /// commit in flight are not among them: quitting does not simply
    /// discard those, and `committing` says what becomes of them.
    pub(crate) staged: usize,
    /// How many tables hold them.
    pub(crate) tables: usize,
    /// Changes held after a commit that got no answer (docs/EDITING.md,
    /// "Commit"): they may already be in the database, so they are not
    /// called uncommitted.
    pub(crate) held: usize,
    /// A cell editor is open: what is typed in it is staged only when it
    /// is confirmed.
    pub(crate) editing: bool,
    /// A commit is in flight.
    pub(crate) committing: bool,
    /// The Query view holds a transaction open.
    pub(crate) transaction: bool,
    /// A statement is running in the Query view.
    pub(crate) running: bool,
}

/// The one dialog (docs/EDITING.md, "Dialogs"): its message, its detail,
/// and the label of the button that quits. Cancel is the other button, and
/// the default.
#[derive(Debug, PartialEq)]
pub(crate) struct QuitQuestion {
    pub(crate) message: String,
    pub(crate) detail: String,
    pub(crate) confirm: &'static str,
}

impl QuitRisks {
    /// The question to ask before quitting, or None when quitting loses
    /// nothing.
    pub(crate) fn question(&self) -> Option<QuitQuestion> {
        let changes = match self.staged {
            0 => None,
            1 => Some("1 staged change".to_string()),
            n => Some(format!("{n} staged changes")),
        };
        let mut detail = Vec::new();
        if let Some(changes) = &changes {
            let (verb, them) = if self.staged == 1 { ("has", "it") } else { ("have", "them") };
            let place = if self.tables > 1 { format!(" in {} tables", self.tables) } else { String::new() };
            detail.push(format!(
                "{changes}{place} {verb} not been committed, and quitting discards {them}."
            ));
        }
        if self.held > 0 {
            let (them, are, they) =
                if self.held == 1 { ("1 change", "is", "it") } else { ("changes", "are", "they") };
            let them = if self.held == 1 { them.to_string() } else { format!("{} {them}", self.held) };
            detail.push(format!(
                "{them} {are} held after a commit that got no answer: {they} may already be in \
                 the database, and quitting drops the held copy."
            ));
        }
        if self.editing {
            detail.push(
                "A cell editor is open: what is typed in it is not staged, and quitting discards it."
                    .to_string(),
            );
        }
        if self.committing {
            detail.push(
                "A commit is still running. Quitting ends it unreported: its changes land only \
                 if the server already has its COMMIT, and are rolled back otherwise."
                    .to_string(),
            );
        }
        if self.transaction {
            detail.push("The Query view holds a transaction open, and quitting rolls it back.".to_string());
        }
        if self.running {
            detail.push(
                "A statement is still running in the Query view. Quitting leaves its outcome \
                 unreported."
                    .to_string(),
            );
        }
        if detail.is_empty() {
            return None;
        }
        // Staged changes alone are the common case, and the question names
        // them; anything else is asked plainly, with the facts below it.
        let only_staged =
            !(self.held > 0 || self.editing || self.committing || self.transaction || self.running);
        let message = match (&changes, only_staged) {
            (Some(changes), true) => format!("Discard {changes} and quit?"),
            _ => "Quit DuckTable?".to_string(),
        };
        Some(QuitQuestion {
            message,
            detail: detail.join(" "),
            confirm: if only_staged { "Discard and Quit" } else { "Quit Anyway" },
        })
    }
}

/// Which database a row, a connection or a connect in flight is. A name
/// does not say: two files can share a stem, and a file and a remote can
/// share a name. A database on this machine is its file, by canonical path;
/// a remote is the config entry of its name.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum DbKey {
    File(std::path::PathBuf),
    Remote(String),
}

impl DbKey {
    /// The key of a sidebar row. Canonicalizing reads the filesystem, so a
    /// row's key is made once, where the row is (`refresh`), and compared
    /// from then on.
    pub(crate) fn of_row(name: &str, path: Option<&std::path::Path>) -> Self {
        match path {
            Some(path) => DbKey::of_file(path),
            None => DbKey::Remote(name.to_string()),
        }
    }

    /// The key of a database file, under any spelling of its path.
    pub(crate) fn of_file(path: &std::path::Path) -> Self {
        DbKey::File(harbor_client::paths::canonical_db(path).unwrap_or_else(|_| path.to_path_buf()))
    }

    /// The key of a live connection, whose file is canonical already.
    pub(crate) fn of_conn(conn: &Conn) -> Self {
        Self::of_parts(&conn.name, conn.db.as_deref())
    }

    /// `of_conn`, from the two facts it reads.
    fn of_parts(name: &str, db: Option<&std::path::Path>) -> Self {
        match db {
            Some(db) => DbKey::File(db.to_path_buf()),
            None => DbKey::Remote(name.to_string()),
        }
    }

    /// An element id for the row: distinct for every row the sidebar lists.
    pub(crate) fn element_id(&self) -> String {
        match self {
            DbKey::File(path) => format!("file:{}", path.display()),
            DbKey::Remote(name) => format!("remote:{name}"),
        }
    }
}

pub(crate) struct RowVm {
    pub(crate) name: String,
    /// Which database the row is. Everything that must tell rows apart goes
    /// by this, never by the name: the highlight, a Stop in flight, the
    /// row's own element.
    pub(crate) key: DbKey,
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

/// What a connect was aimed at: kept by a failed one, so Retry dials the
/// same thing instead of looking a name up again.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Aim {
    /// A sidebar row, which is the database it shows: a file on this
    /// machine under the row's name, or the remote the config calls by it.
    Row { name: String, path: Option<std::path::PathBuf> },
    /// File → Open, or a drop: the path alone.
    File(std::path::PathBuf),
}

pub(crate) enum Phase {
    Idle,
    Connected {
        conn: Conn,
        info: wire::InfoResponse,
        /// The one snapshot everything schema-shaped renders from: tables,
        /// columns, DDL, exact row counts, and the file's size on disk all
        /// arrive in this single document (harbor 0.18+).
        catalog: harbor_client::Catalog,
    },
    Failed { name: String, message: String, aim: Aim },
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
    /// Which database that connect is aimed at, for the sidebar's highlight:
    /// of two rows that share a name, only the one clicked lights up.
    pub(crate) connecting_key: Option<DbKey>,
    /// The sidebar's table-name filter; Some = the field is open.
    pub(crate) table_filter: Option<Entity<gpui_kit::component::input::InputState>>,
    /// The sidebar's database-name filter; Some = the field is open.
    pub(crate) berth_filter: Option<Entity<gpui_kit::component::input::InputState>>,
    /// The quit dialog is on screen. A second ⌘Q, or a click on the close
    /// button, while it is up asks nothing more.
    pub(crate) asking_to_quit: bool,
    /// Fence for table selection: a first-page fetch that finishes after a
    /// newer click discards itself instead of swapping in a stale grid.
    select_seq: u64,
    /// A table chosen while the current grid was committing. Swapping then
    /// would park a staged set whose statements are already running: back
    /// on the table, it would come back staged, and ⌘S would write it twice.
    /// The switch runs when the commit settles (`CommitSettled`).
    deferred_select: Option<(String, String)>,
    /// Fence for the berth-list refresh: overlapping sweeps (a manual
    /// click racing the one connect fires) commit newest-wins instead of
    /// arbitrary order.
    refresh_seq: u64,
    /// Fence for catalog refreshes. Writes and manual refreshes can overlap;
    /// only the newest `/catalog` response may replace the sidebar snapshot.
    catalog_seq: u64,
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
    /// The sidebar/content divider (DESIGN.md: divider positions persist —
    /// the width saves at the end of each drag).
    pub(crate) sidebar_resize: Entity<gpui_kit::component::resizable::ResizableState>,
    /// Berths with a Stop in flight: the row keeps its slot but swaps its
    /// dot for a spinner and stops taking clicks until the shutdown lands.
    pub(crate) stopping: std::collections::HashSet<DbKey>,
    /// Berths mid-departure: the shutdown returned and the survey no
    /// longer reports them, but the row lingers one fade before it's
    /// dropped. `refresh` re-splices these so the survey's removal can't
    /// yank a row out from under its own fade-out.
    pub(crate) leaving: std::collections::HashSet<DbKey>,
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
            cx.new(|_| gpui_kit::component::resizable::ResizableState::default());
        cx.subscribe(
            &sidebar_resize,
            |_, state, _: &gpui_kit::component::resizable::ResizablePanelEvent, cx| {
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
            connecting_key: None,
            table_filter: None,
            berth_filter: None,
            asking_to_quit: false,
            select_seq: 0,
            deferred_select: None,
            refresh_seq: 0,
            catalog_seq: 0,
            warning: None,
            query: None,
            staged: std::collections::HashMap::new(),
            sidebar_resize,
            stopping: std::collections::HashSet::new(),
            leaving: std::collections::HashSet::new(),
            path_copy: None,
            installed_version: None,
        };
        // Every way out of the app gives its sessions back, the ones that
        // ask nothing included: Quit from the Dock, a logout, the updater's
        // relaunch. The release is the whole hook, so the future it hands
        // back has nothing left to do.
        cx.on_app_quit(|this, cx| {
            this.release_for_quit(cx);
            async {}
        })
        .detach();
        this.refresh(cx);
        this
    }

    /// What a quit would lose right now (`QuitRisks`).
    pub(crate) fn quit_risks(&self, cx: &App) -> QuitRisks {
        let grid = self.grid.as_ref().map(|g| g.read(cx));
        let committing = grid.is_some_and(|g| g.committing);
        // Every staged set the window holds: the grid's own, unless a
        // commit has it in flight, and those parked by a table switch.
        let on_screen = grid.filter(|_| !committing).into_iter().flat_map(|g| g.staged_sets());
        let sets: Vec<&crate::edits::Edits> =
            self.staged.values().chain(on_screen).filter(|e| e.any_staged()).collect();
        let staged: Vec<usize> = sets.iter().filter(|e| !e.in_doubt()).map(|e| e.len()).collect();
        QuitRisks {
            staged: staged.iter().sum(),
            tables: staged.len(),
            held: sets.iter().filter(|e| e.in_doubt()).map(|e| e.len()).sum(),
            editing: grid.is_some_and(|g| g.is_editing()),
            committing,
            transaction: self.query.as_ref().is_some_and(|q| q.read(cx).in_transaction()),
            running: self.query.as_ref().is_some_and(|q| q.read(cx).is_running()),
        }
    }

    /// The quit dialog opens. A connect still in flight is called off: its
    /// landing would replace the grid, the query and every staged edit
    /// under the dialog, and Cancel must find them as they were.
    pub(crate) fn quit_dialog_opened(&mut self, cx: &mut Context<Self>) {
        self.asking_to_quit = true;
        self.cancel(cx);
    }

    /// The quit dialog was cancelled. What waited for it runs: a table
    /// switch that landed under it, and the fleet's reconciliation, which
    /// drops a connection whose server stopped meanwhile.
    pub(crate) fn quit_dialog_cancelled(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.asking_to_quit = false;
        if let Some((schema, name)) = self.deferred_select.take() {
            self.select_table(schema, name, window, cx);
        }
        self.refresh(cx);
    }

    /// The key of the row called `name` with this `path`, as the fleet's
    /// survey made it. A row's key is canonical, and making one reads the
    /// filesystem, which this thread does not: a path no row has is keyed
    /// as it is spelled.
    fn key_of(&self, name: &str, path: Option<&std::path::Path>) -> DbKey {
        self.rows
            .iter()
            .find(|r| r.path.as_deref() == path && (path.is_some() || r.name == name))
            .map(|r| r.key.clone())
            .unwrap_or_else(|| match path {
                Some(path) => DbKey::File(path.to_path_buf()),
                None => DbKey::Remote(name.to_string()),
            })
    }

    /// The user chose to quit: give back every session this window holds,
    /// so the server ends what runs on them at once instead of at their
    /// timeouts. They are the Query view's transaction, open or still
    /// opening, and the grid's commit in flight. Each release is one
    /// request; they run side by side, and the quit waits for them briefly
    /// and no longer, since the server reclaims an abandoned session itself.
    pub(crate) fn release_for_quit(&mut self, cx: &mut Context<Self>) {
        let mut releases: Vec<Box<dyn FnOnce() + Send>> = Vec::new();
        if let Some(query) = &self.query {
            releases.extend(query.update(cx, |q, _| q.release_for_quit()));
        }
        if let Some(grid) = &self.grid {
            releases.extend(grid.read(cx).release_for_quit());
        }
        let (done, wait) = std::sync::mpsc::channel();
        let count = releases.len();
        for release in releases {
            let done = done.clone();
            std::thread::spawn(move || {
                release();
                let _ = done.send(());
            });
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
        for _ in 0..count {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if wait.recv_timeout(left).is_err() {
                break;
            }
        }
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
        // Under the quit dialog nothing moves: the table keys still reach
        // here, and Cancel must find the table that was on screen.
        if self.asking_to_quit {
            return;
        }
        // The switch replaces the grid, and an open editor with it. Its text
        // is staged first, to be parked with the rest; text the column
        // refuses keeps the editor open with the reason, and the table.
        if let Some(grid) = self.grid.clone()
            && !grid.update(cx, |grid, cx| grid.settle_editor(cx))
        {
            return;
        }
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
            let rowid = structure.as_ref().is_some_and(|s| s.keyed_by_rowid());
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
                // A switch that would land during a commit, or under the
                // quit dialog, waits for it.
                if state.asking_to_quit || state.grid.as_ref().is_some_and(|g| g.read(cx).committing) {
                    state.deferred_select = Some((clone_str(&schema), clone_str(&name)));
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
                cx.subscribe(&grid, |state, _, _: &CatalogRefreshRequested, cx| {
                    state.refresh_catalog(cx)
                })
                .detach();
                cx.subscribe_in(&grid, window, |state, _, _: &CommitSettled, window, cx| {
                    if let Some((schema, name)) = state.deferred_select.take() {
                        state.select_table(schema, name, window, cx);
                    }
                })
                .detach();
                // And returning to a table hands its parked edits back.
                // The grid owns them from here: one whose first fetch
                // failed has no columns to judge them by, keeps them until
                // a fetch brings some, and surrenders them again through
                // `take_edits` if it is replaced first.
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
                if !state
                    .query
                    .as_ref()
                    .is_some_and(|q| q.read(cx).is_for(&berth))
                {
                    let qconn = grid.read(cx).conn.clone();
                    let query =
                        cx.new(|cx| crate::query::QueryView::new(qconn, &berth, window, cx));
                    cx.subscribe(&query, |state, _, _: &CatalogRefreshRequested, cx| {
                        state.refresh_tables(cx)
                    })
                    .detach();
                    state.query = Some(query);
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
    ) -> Entity<gpui_kit::component::input::InputState> {
        let input = cx.new(|cx| {
            gpui_kit::component::input::InputState::new(window, cx).placeholder(placeholder)
        });
        cx.subscribe(&input, |_, _, _: &gpui_kit::component::input::InputEvent, cx| {
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
        self.catalog_seq += 1;
        let attempt = self.attempt;
        let refresh = self.catalog_seq;
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move { harbor_client::catalog(&conn) })
                .await;
            this.update(cx, |state, cx| {
                if !catalog_refresh_is_current(state.attempt, state.catalog_seq, attempt, refresh) {
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

    /// Refresh every table-facing snapshot: exact catalog counts in the
    /// sidebar and the currently open Data page. Query results are an
    /// explicit SQL snapshot and are intentionally left unchanged.
    pub(crate) fn refresh_tables(&mut self, cx: &mut Context<Self>) {
        // Under the quit dialog nothing behind it moves: a refresh would
        // confirm an open editor and replace the page.
        if self.asking_to_quit {
            return;
        }
        self.refresh_catalog(cx);
        if let Some(grid) = self.grid.clone() {
            grid.update(cx, |grid, cx| grid.refresh_current(cx));
        }
    }

    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        self.refresh_seq += 1;
        let fence = self.refresh_seq;
        // The connected berth's catalog is already in hand; its row must
        // not pay a second connect + catalog download just for a count.
        let connected: Option<(DbKey, usize)> = match &self.phase {
            Phase::Connected { conn, catalog, .. } => {
                Some((DbKey::of_conn(conn), catalog.tables.len()))
            }
            _ => None,
        };
        cx.spawn(async move |this, cx| {
            // survey() answers liveness from each server's own socket:
            // a listening socket is the registration, so a running
            // database the config does not know is a row too.
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
                        // The connection is this row only if it is the same
                        // database: the same file, or the same remote.
                        let key = DbKey::of_row(&row.name, row.path.as_deref());
                        let here = known.filter(|(connected, _)| *connected == key);
                        let connected_here = here.is_some();
                        let tables = match here {
                            Some((_, count)) => Some(count),
                            None => row
                                .state
                                .is_live()
                                .then(|| {
                                    // The row is dialed as what it shows, and a
                                    // count never starts a server or a tunnel:
                                    // a file is joined only if it is running,
                                    // and a remote is live here only when its
                                    // url answers on this machine.
                                    let conn = match &row.path {
                                        Some(path) => fleet::join_file(&row.name, path)?,
                                        None => fleet::connect_remote(&row.name).ok()?,
                                    };
                                    // Lite: this sweep only counts tables,
                                    // so it never pays for columns or DDL.
                                    let cat = harbor_client::catalog_lite(&conn).ok()?;
                                    Some(cat.tables.len())
                                })
                                .flatten(),
                        };
                        RowVm {
                            state: if connected_here { State::Running } else { row.state },
                            attached: row.attached,
                            autostart: row.autostart,
                            path: row.path,
                            tables,
                            size: row.size,
                            note: row.note,
                            key,
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
                        if state.leaving.contains(&r.key)
                            && !rows.iter().any(|n| n.key == r.key)
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
                    Phase::Connected { conn, .. } => {
                        Some((clone_str(&conn.name), DbKey::of_conn(conn)))
                    }
                    _ => None,
                };
                // Under the quit dialog the connection is left as it is:
                // dropping it clears the grid and every staged edit, and
                // Cancel must find them. The refresh that follows a cancel
                // reconciles.
                if let Some((name, key)) = connected
                    && !state.asking_to_quit
                    && !state.rows.iter().any(|r| r.key == key && r.state.is_live())
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
    ///
    /// A sidebar row connects to what it shows: its file when it has one,
    /// the config's remote of its name otherwise. The name alone is never
    /// looked up again, because a local file and a remote can share one.
    pub(crate) fn connect_row(
        &mut self,
        name: String,
        path: Option<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let aim = Aim::Row { name: clone_str(&name), path: path.clone() };
        self.dial(
            clone_str(&name),
            aim,
            move || {
                let conn = match &path {
                    Some(path) => fleet::connect_file(&name, path)?,
                    None => fleet::connect_remote(&name)?,
                };
                let info = fleet::info(&conn)?;
                let catalog = harbor_client::catalog(&conn)?;
                Ok((conn, info, catalog))
            },
            cx,
        );
    }

    /// Dial again what a failed connect was aimed at.
    pub(crate) fn retry(&mut self, cx: &mut Context<Self>) {
        match &self.phase {
            Phase::Failed { aim: Aim::Row { name, path }, .. } => {
                self.connect_row(clone_str(name), path.clone(), cx)
            }
            Phase::Failed { aim: Aim::File(path), .. } => self.open_path(path.clone(), cx),
            _ => {}
        }
    }

    /// Whether `row` is the database on screen, or the one being dialed:
    /// the sidebar's highlight.
    pub(crate) fn is_active_row(&self, row: &RowVm) -> bool {
        let connected = match &self.phase {
            Phase::Connected { conn, .. } => Some(DbKey::of_conn(conn)),
            _ => None,
        };
        active_key(self.connecting_key.as_ref(), connected.as_ref()) == Some(&row.key)
    }

    /// File → Open Database URL: persist the named port, then connect
    /// through the same path a sidebar click uses. A failed dial still leaves
    /// the database saved so Retry has something durable to target.
    pub(crate) fn add_database(
        &mut self,
        name: String,
        host: String,
        port: String,
        cx: &mut Context<Self>,
    ) {
        let shown = harbor_client::paths::normalize(&name).unwrap_or(name);
        self.dial(
            clone_str(&shown),
            Aim::Row { name: clone_str(&shown), path: None },
            move || {
                let name = fleet::add_database(&shown, &host, &port)?;
                let conn = fleet::connect_remote(&name)?;
                let info = fleet::info(&conn)?;
                let catalog = harbor_client::catalog(&conn)?;
                Ok((conn, info, catalog))
            },
            cx,
        );
    }

    /// Forget a port-based database and close its tunnel if it is the one on
    /// screen. No remote shutdown is sent: removing connection details must
    /// never mutate the database they point at.
    pub(crate) fn remove_remote_database(&mut self, name: String, cx: &mut Context<Self>) {
        if self.asking_to_quit {
            return;
        }
        let connected_here = matches!(
            &self.phase,
            Phase::Connected { conn, .. } if DbKey::of_conn(conn) == DbKey::Remote(clone_str(&name))
        );
        if connected_here {
            self.drop_connection(cx);
        }
        self.fleet_then_refresh(move || fleet::remove_remote(&name), cx);
    }

    /// The shared spine of connect / open_path: show `shown` as the connecting
    /// label under a fresh fence, run `dial` (raise-or-join the server and read
    /// its catalog) on a background thread, then swap the pane to the outcome
    /// in one frame. A stale fence discards itself, so a slow attempt never
    /// clobbers a newer one; current content keeps rendering until it lands.
    fn dial<F>(&mut self, shown: String, aim: Aim, dial: F, cx: &mut Context<Self>)
    where
        F: FnOnce() -> Result<(Conn, wire::InfoResponse, harbor_client::Catalog), String>
            + Send
            + 'static,
    {
        // The quit dialog promises that Cancel leaves everything as it was.
        // A connect replaces the grid, the query and every staged edit, and
        // the menu bar and a file drop still reach it under the dialog.
        if self.asking_to_quit {
            return;
        }
        self.attempt += 1;
        let fence = self.attempt;
        self.connecting = Some(clone_str(&shown));
        self.connecting_key = Some(match &aim {
            Aim::Row { name, path } => self.key_of(name, path.as_deref()),
            Aim::File(path) => self.key_of("", Some(path)),
        });
        cx.notify();
        cx.spawn(async move |this, cx| {
            let outcome = cx.background_executor().spawn(async move { dial() }).await;
            this.update(cx, |state, cx| {
                if state.attempt != fence {
                    return;
                }
                state.connecting = None;
                state.connecting_key = None;
                state.selected_table = None;
                state.grid = None;
                state.query = None;
                state.staged.clear();
                state.deferred_select = None;
                state.select_seq += 1;
                state.phase = match outcome {
                    Ok((conn, info, catalog)) => Phase::Connected { conn, info, catalog },
                    Err(message) => Phase::Failed { name: clone_str(&shown), message, aim },
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
            Aim::File(path.clone()),
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
        // A switch that waited on this berth's commit belongs to it too: the
        // grid it waited on is gone, and no CommitSettled will come for it.
        self.deferred_select = None;
        self.select_seq += 1;
        self.sync_path_copy(cx);
    }

    /// Stop a berth's server — the close half of open. Right-click → Stop
    /// lands here: POST /shutdown to that file's server, then refresh so its
    /// row goes from green to stopped (or leaves, if it was ephemeral). If
    /// the berth we're viewing is the one stopped, the view returns to Idle
    /// — a stopped server has nothing to show.
    pub(crate) fn stop_berth(
        &mut self,
        name: String,
        path: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) {
        if self.asking_to_quit {
            return;
        }
        // The file is the whole target: of two databases that share a
        // name, Stop reaches the one whose row was clicked and no other.
        let key = self.key_of(&name, Some(&path));
        // Idempotent: a second Stop while one is already in flight (or the
        // row is already fading out) is a no-op.
        if self.stopping.contains(&key) || self.leaving.contains(&key) {
            return;
        }
        let connected_here = matches!(
            &self.phase,
            Phase::Connected { conn, .. } if DbKey::of_conn(conn) == key
        );
        // The row keeps its slot and spins while the shutdown runs.
        self.stopping.insert(key.clone());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let outcome =
                cx.background_executor().spawn(async move { fleet::stop(&path) }).await;
            let stopped = this
                .update(cx, |state, cx| {
                    state.stopping.remove(&key);
                    match outcome {
                        Err(message) => {
                            // The berth is still alive — no fade, no reset.
                            state.warning = Some(format!("{name}: {message}"));
                            state.refresh(cx);
                            cx.notify();
                            false
                        }
                        Ok(()) => {
                            // It departed: hold the row for one fade, then
                            // let refresh's survey drop it for real.
                            state.leaving.insert(key.clone());
                            // The world we were showing just departed.
                            // Under the quit dialog it stays on screen until
                            // the dialog is answered (`quit_dialog_cancelled`).
                            if connected_here && !state.asking_to_quit {
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
            // drop the ghost so the gap closes. A survey that landed in the
            // meantime may show the database again, as a stopped row of the
            // config: only a row still running is the ghost.
            cx.background_executor().timer(std::time::Duration::from_millis(260)).await;
            this.update(cx, |state, cx| {
                state.leaving.remove(&key);
                state.rows.retain(|r| !(r.key == key && r.state.is_live()));
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
        self.connecting_key = None;
        cx.notify();
    }

}

impl DuckTable {
    /// The carousel landed on Query: hand focus to the editor (the
    /// symmetry of landing on Data focusing the grid).
    pub(crate) fn focus_query(&self, cx: &mut gpui_kit::App) {
        if let Some(q) = &self.query {
            q.update(cx, |q, cx| q.request_focus(cx));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{active_key, catalog_refresh_is_current, DbKey, QuitRisks};
    use std::path::Path;

    #[test]
    fn rows_and_connections_are_told_apart_by_database_not_by_name() {
        // Paths that do not exist canonicalize to themselves.
        let here = DbKey::of_row("a", Some(Path::new("/nonexistent-dt/data/a.duckdb")));
        let there = DbKey::of_row("a", Some(Path::new("/nonexistent-dt/tmp/a.duckdb")));
        let remote = DbKey::of_row("a", None);
        assert_eq!(here, DbKey::File("/nonexistent-dt/data/a.duckdb".into()));
        assert_eq!(remote, DbKey::Remote("a".into()));
        // Three rows called `a`: two files and a remote, three databases.
        assert!(here != there && here != remote && there != remote);
        // And three element ids.
        let ids = [here.element_id(), there.element_id(), remote.element_id()];
        assert!(ids[0] != ids[1] && ids[0] != ids[2] && ids[1] != ids[2], "{ids:?}");

        // A connection is the row of its own file, whatever either is named:
        // a file opened by path is named for its stem, and its row for the
        // name its server reports.
        let conn = DbKey::of_parts("inventory", Some(Path::new("/nonexistent-dt/data/a.duckdb")));
        assert_eq!(conn, here);
        assert_ne!(conn, there);
        assert_ne!(DbKey::of_parts("a", Some(Path::new("/nonexistent-dt/tmp/a.duckdb"))), here);
        // A remote connection is the remote row of its name, never a file's.
        assert_eq!(DbKey::of_parts("a", None), remote);
        assert_ne!(DbKey::of_parts("a", None), here);
        assert_ne!(DbKey::of_parts("b", None), remote);
        // One file under two spellings is one database.
        let tmp = std::env::temp_dir();
        assert_eq!(DbKey::of_file(&tmp.join("x.duckdb")), DbKey::of_file(&tmp.join(".").join("x.duckdb")));
    }

    #[test]
    fn the_lit_row_is_the_one_being_dialed_else_the_connected_one() {
        let (file, remote) = (DbKey::File("/data/a.duckdb".into()), DbKey::Remote("a".into()));
        assert_eq!(active_key(None, None), None);
        assert_eq!(active_key(None, Some(&file)), Some(&file));
        // A click on the remote row of the same name lights that row, not
        // the connected file's.
        assert_eq!(active_key(Some(&remote), Some(&file)), Some(&remote));
        assert_eq!(active_key(Some(&remote), None), Some(&remote));
    }

    #[test]
    fn quitting_asks_only_when_something_would_be_lost() {
        assert_eq!(QuitRisks::default().question(), None);

        let one = QuitRisks { staged: 1, tables: 1, ..Default::default() }.question().unwrap();
        assert_eq!(one.message, "Discard 1 staged change and quit?");
        assert_eq!(one.detail, "1 staged change has not been committed, and quitting discards it.");
        assert_eq!(one.confirm, "Discard and Quit");

        // Edits parked for a table that is not on screen count like any other.
        let parked = QuitRisks { staged: 5, tables: 2, ..Default::default() }.question().unwrap();
        assert_eq!(parked.message, "Discard 5 staged changes and quit?");
        assert_eq!(
            parked.detail,
            "5 staged changes in 2 tables have not been committed, and quitting discards them."
        );

        // A commit in flight is never abandoned without a word, staged
        // changes or not; nor is a transaction the Query view holds.
        let committing = QuitRisks { committing: true, ..Default::default() }.question().unwrap();
        assert_eq!(committing.message, "Quit DuckTable?");
        assert!(committing.detail.starts_with("A commit is still running."));
        // Its changes are not called discarded: they may land.
        assert!(committing.detail.ends_with("and are rolled back otherwise."));
        assert!(!committing.detail.contains("discards"));
        assert_eq!(committing.confirm, "Quit Anyway");
        let all = QuitRisks {
            staged: 3,
            tables: 1,
            held: 0,
            editing: true,
            committing: true,
            transaction: true,
            running: true,
        }
        .question()
        .unwrap();
        assert_eq!(all.message, "Quit DuckTable?");
        assert!(all.detail.starts_with("3 staged changes have not been committed"));
        assert!(all.detail.contains("A cell editor is open"));
        assert!(all.detail.contains("A commit is still running."));
        assert!(all.detail.contains("The Query view holds a transaction open, and quitting rolls it back."));
        assert!(all.detail.ends_with("Quitting leaves its outcome unreported."));
        assert_eq!(all.confirm, "Quit Anyway");
        // A set held after a commit that got no answer is not called
        // uncommitted: it may have landed.
        let held = QuitRisks { held: 3, ..Default::default() }.question().unwrap();
        assert_eq!((held.message.as_str(), held.confirm), ("Quit DuckTable?", "Quit Anyway"));
        assert_eq!(
            held.detail,
            "3 changes are held after a commit that got no answer: they may already be in the \
             database, and quitting drops the held copy."
        );
        assert!(!held.detail.contains("not been committed"));
        let one = QuitRisks { staged: 2, tables: 1, held: 1, ..Default::default() }.question().unwrap();
        assert!(one.detail.starts_with("2 staged changes have not been committed, and quitting discards them."));
        assert!(one.detail.contains("1 change is held after a commit that got no answer: it may already"));
        // Text in an open editor, and a statement in flight, are each
        // reason enough to ask.
        let typing = QuitRisks { editing: true, ..Default::default() }.question().unwrap();
        assert_eq!((typing.message.as_str(), typing.confirm), ("Quit DuckTable?", "Quit Anyway"));
        let running = QuitRisks { running: true, ..Default::default() }.question().unwrap();
        assert!(running.detail.starts_with("A statement is still running in the Query view."));
    }

    #[test]
    fn catalog_refresh_accepts_only_the_current_connection_and_request() {
        assert!(catalog_refresh_is_current(7, 11, 7, 11));
        assert!(!catalog_refresh_is_current(8, 11, 7, 11));
        assert!(!catalog_refresh_is_current(7, 12, 7, 11));
    }
}
