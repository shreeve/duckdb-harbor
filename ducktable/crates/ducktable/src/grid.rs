//! The results grid: gpui-component's virtualized Table underneath (its
//! two-axis virtualization stress-checked by `examples/wide_probe.rs`, a
//! plain-cell probe of the library — not of this delegate), DuckTable's
//! delegate on top.
//! This surface owns fetching (server-side pages via `POST /sql`), value
//! presentation, its header/status strips, and inline editing (the cell
//! editor and staging here, the edit model in `edits.rs`).
//!
//! Rows arrive as explicit pages: a fetched page REPLACES the rows in one
//! frame (DESIGN.md: fetch first, commit over the old value), so the grid
//! always shows one internally consistent snapshot. Row indices here are
//! display positions within the current page; a staged change is keyed by
//! its row's identity (`edits.rs`) and projected onto whatever page is
//! showing, never stored by index.

use crate::chrome::{icon_tile, toggle_tile};
use crate::edits::{self, Edits};
use crate::prefs::{self, ViewMode};
use crate::theme::{
    pal, ui_font, value_font, CELL_TEXT, GUTTER_TEXT, HEADER_TEXT, PANE_INSET, TAG_TEXT,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use gpui_kit::component::input::{IndentInline, OutdentInline};
use gpui_kit::component::table::{Column as TableColumn, DataTable, TableDelegate, TableState};
use gpui_kit::component::tooltip::Tooltip;
// Base's splitters: a plain hairline that takes the drag color while held.
// The component library's own add a pill that grows on hover.
use gpui_kit::base::h_resizable;
use gpui_kit::component::resizable::{resizable_panel, ResizablePanelEvent, ResizableState};
use gpui_kit::component::{Sizable as _, StyledExt as _};
use harbor_client::Conn;
use serde_json::Value;
use std::rc::Rc;

// Page sizes live in prefs::PAGE_SIZES (default 500); explicit pages give
// ordinary tables a boundary-free read and huge tables honest jumps,
// constant memory, and consistent snapshots — infinite append would
// silently stitch separately-queried chunks together.

/// The DataTable binds home, end, page up/down and tab to its own
/// selection actions, and bindings dispatch before the grid's key
/// listener, which owns those keys (ring moves and Sheets' tab walk). A
/// NoAction binding in the same context leaves them to the listener.
pub(crate) fn init(cx: &mut App) {
    cx.bind_keys(
        ["home", "end", "pageup", "pagedown", "tab", "shift-tab"]
            .map(|key| KeyBinding::new(key, NoAction {}, Some("DataTable"))),
    );
}

// 7 is Menlo's digit advance at GUTTER_TEXT (11px); 16 is the gutter's
// horizontal padding. Both move if the value font or size does. The
// floor is TWO digits — room for "99" — so the gutter starts identical
// across Structure, Data, and Query and widens only when the content
// actually holds bigger row numbers (a 45,000th row earns five digits;
// a five-row table never pays for them).
pub(crate) fn gutter_width(max_row: u64) -> f32 {
    let digits = (max_row.max(1).ilog10() as f32 + 1.).max(2.);
    16. + digits * 7.
}

pub(crate) struct Grid {
    // `table`, `filter_input`, and `col_search` are pub(crate) for the
    // satellite `impl Grid` files (footer.rs); nothing outside those
    // renders should touch them.
    pub(crate) table: Entity<TableState<GridDelegate>>,
    pub(crate) conn: Conn,
    /// The Query view's open transaction, on a results grid whose statement
    /// ran inside it: its later pages are read on the same session, so they
    /// see what the transaction has written. None everywhere else.
    pub(crate) session: Option<crate::query::Txn>,
    /// The berth's Query view, injected by the app (berth-scoped, so it
    /// outlives this table's grid); rendered by the Query segment.
    pub(crate) query_view: Option<Entity<crate::query::QueryView>>,
    /// Repaints the footer's status line as the query view's run state
    /// ticks and settles; replaced whole when a berth swaps views in.
    pub(crate) query_obs: Option<Subscription>,
    /// This grid is someone else's results pane (the Query view): it
    /// renders body only — no title strip, no filter, no footer. The
    /// host owns the chrome; the app footer reads THIS grid through it.
    embedded: bool,
    /// Whether page fetches make sense for this source: true for tables
    /// and SELECT-shaped queries (pageable as a wrapped subquery), false
    /// for a statement whose entire result already sits on page 0.
    pub(crate) pageable: bool,
    /// The FROM target this grid pages: a quoted `"schema"."table"`, or
    /// a parenthesized user statement (sql::query_source).
    source: String,
    title: String,
    /// Current page (0-based) and the size its rows were fetched with.
    pub(crate) page: usize,
    pub(crate) page_size: usize,
    /// Raw SQL WHERE clause text, verbatim from the filter strip.
    pub(crate) filter: Option<String>,
    /// Exact server-side row count (under the current filter), when the
    /// count query succeeded.
    pub(crate) total_rows: Option<u64>,
    error: Option<String>,
    pub(crate) last_time_ms: u64,
    /// The staging layer (docs/EDITING.md), present only when the table
    /// is editable: its rows have an identity, a primary key or DuckDB's
    /// rowid. None = read-only.
    pub(crate) edits: Option<Edits>,
    /// The open cell editor, if any. Provisional input lives here; it
    /// becomes a staged change only on confirm.
    editor: Option<CellEditor>,
    /// The identity is DuckDB's implicit rowid (no catalog key): pages
    /// fetch it paired with the row's hash, and the delegate hides schema
    /// column 0.
    rowid: bool,
    /// Primary-key column names from the catalog — kept so an error-born
    /// grid can build its Edits when its first schema finally lands.
    pk_cols: Vec<String>,
    /// NOT NULL per schema column (from the catalog, by name) — staging
    /// NULL into one refuses at the fingers, not at the server.
    not_null: Vec<bool>,
    /// Declared defaults per schema column. A missing draft cell is
    /// omitted from INSERT; this tells validation whether NOT NULL is
    /// already satisfied by the engine.
    defaults: Vec<Option<String>>,
    /// Generated columns are visible but never writable. DuckDB computes
    /// them from the supplied columns and INSERT RETURNING exposes them.
    generated: Vec<bool>,
    /// The staged set parked for this table, handed over while the grid had
    /// no columns to judge it by (its first fetch failed). The fetch that
    /// brings a schema adopts it, and a grid replaced before then gives it
    /// back (`take_edits`), so a failed fetch never costs the staged edits.
    parked: Option<Edits>,
    /// A fetch found the table with other columns than this grid's while
    /// edits were staged against these. The page on screen stays, nothing
    /// more is staged and nothing commits until the staged set is empty;
    /// the fetch that follows adopts the table as it is.
    reshaped: bool,
    /// A commit is in flight. Until it resolves, ⌘S is a no-op, nothing is
    /// staged, undone or discarded (its statements were built at ⌘S), and a
    /// table switch waits for it (`app::CommitSettled`). It stays up through
    /// the page fetch that follows the commit (`post_commit`): until that
    /// lands, the rows on screen carry the identities they were fetched
    /// with, and a commit that re-keyed a row has made those stale.
    pub(crate) committing: bool,
    /// The commit has answered and its page fetch is in flight; what the
    /// fetch's landing settles (`settle_commit`).
    post_commit: Option<PostCommit>,
    /// A commit landed and the fetch after it failed, so the page on screen
    /// is from before the commit and its row identities may name other rows
    /// or none. Nothing is staged against it; the next fetch that lands
    /// lowers the flag.
    unrefreshed: bool,
    /// The session of the commit in flight, from its opening to its release:
    /// a quit in between gives it back, so nothing is left open on the
    /// server (`release_for_quit`).
    commit_session: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// Focus should return to the table on the next frame — set by paths
    /// that lack a Window (subscriptions), consumed by render.
    needs_focus: bool,
    /// The ring's seat across a keyboard page flip (page_step): the
    /// fetch that lands consumes it, row clamped to the new page.
    ring_keep: Option<(usize, usize)>,
    /// A horizontal scroll was requested: the table applies it while
    /// painting its BODY, after the header row has already painted, so
    /// the scrolling frame shows a stale header. Render consumes this
    /// by scheduling one more frame, where the header catches up.
    header_chase: bool,
    /// The keystroke interceptor that keeps the table's built-in
    /// arrow/escape bindings out of navigation (see Grid::new) — held so
    /// it unregisters when this grid drops.
    _intercept: Subscription,
    /// The filter strip's input; Some = the strip is open.
    pub(crate) filter_input: Option<Entity<gpui_kit::component::input::InputState>>,
    /// Escape interceptor for the open filter strip (clear, then
    /// dismiss) — dropped with the strip so closed strips cost nothing.
    filter_esc: Option<Subscription>,
    /// Prefetched with the first page, so switching views is instant.
    structure: Option<crate::structure::TableStructure>,
    /// The Structure view's columns-as-a-grid (structure.rs): the
    /// table's own schema in an embedded, read-only Grid — one grid,
    /// many sources, applied to the catalog itself.
    pub(crate) structure_grid: Option<Entity<Grid>>,
    /// The Structure view's columns/DDL divider (DESIGN.md: divider
    /// positions persist). None when there is no DDL below the columns
    /// — one pane needs no divider.
    pub(crate) structure_split: Option<Entity<ResizableState>>,
    /// The table/inspector divider (DESIGN.md: divider positions persist —
    /// the width saves at the end of each drag).
    resize: Entity<ResizableState>,
    /// The Columns popover's search box — persistent so the query
    /// survives re-renders while the popover is open.
    pub(crate) col_search: Entity<gpui_kit::component::input::InputState>,
    /// The Structure view's DDL block: a READ-ONLY code editor, so the
    /// text is natively selectable (mouse drag, Cmd+C) while every
    /// mutation stays gated off. None when the table has no DDL. It holds
    /// perfectly still: sized to its content, with no room past the last
    /// line and no caret margin to chase. A first-party StyledText block
    /// cannot replace it — it would lose the editor's font metrics,
    /// wrapping layout, and native selection.
    pub(crate) ddl_input: Option<Entity<gpui_kit::component::input::EditorState>>,
    /// The DDL block's copy tile, a self-confirming widget (copy_button.rs).
    pub(crate) ddl_copy: Option<Entity<crate::copy_button::CopyButton>>,
    /// Fence for page fetches: a newer fetch supersedes an older one in
    /// flight, whose outcome is then discarded instead of committing
    /// stale rows.
    fetch_seq: u64,
    /// The table wrapper's window bounds, recorded by a canvas each frame
    /// so `divider_double_click` can hit-test header dividers.
    table_bounds: std::rc::Rc<std::cell::Cell<Bounds<Pixels>>>,

}

impl EventEmitter<crate::app::CatalogRefreshRequested> for Grid {}
impl EventEmitter<crate::app::CommitSettled> for Grid {}

/// Everything a fetch commits along with its rows. The delegate is not
/// touched until the data arrives — the footer, funnel, and gutter always
/// describe the rows actually on screen, and a failed or superseded fetch
/// leaves no half-applied state behind.
struct PageReq {
    page: usize,
    size: usize,
    filter: FilterChange,
    recount: bool,
}

/// One open cell editor. Entry gesture decides arrow physics (docs/
/// EDITING.md): replace entry (typed) — arrows confirm and move the
/// ring; kept-value entry (Enter/double-click) — arrows move the caret.
struct CellEditor {
    row: usize,
    /// Schema column index.
    col: usize,
    /// Present for a synthetic INSERT row; existing rows resolve through
    /// their fetched identity instead.
    draft_key: Option<String>,
    input: Entity<gpui_kit::component::input::InputState>,
    replace: bool,
}

/// What a fetch does to the WHERE filter.
enum FilterChange {
    /// Keep the current filter.
    Keep,
    /// Commit a new one with the rows (None clears it).
    Set(Option<String>),
}

/// What the Table renders from, per cell per frame — and nothing else.
/// The query/session state (page, filter, totals, errors) lives on Grid,
/// which owns the fetches; a page lands here already render-ready.
pub(crate) struct GridDelegate {
    cols: Vec<TableColumn>,
    /// The result schema, kept so the column list can be rebuilt when the
    /// row-number preference flips.
    schema_cols: Vec<wire::Column>,
    /// Display names, one per schema column, derived once at schema commit
    /// — three surfaces (headers, popover, inspector) read them per frame.
    names: Vec<SharedString>,
    /// The gutter's absolute row numbers, derived once per page commit —
    /// render_td must not format per cell per frame.
    row_labels: Vec<SharedString>,
    /// The page's first absolute row (page × size), committed with its
    /// labels — gutter sizing derives from it when the columns rebuild.
    base: usize,
    /// Schema column indices of the identity columns: the primary key's,
    /// or the hidden rowid column of a table without one. Empty when the
    /// source has no row identity (and is therefore read-only).
    pk_ix: Vec<usize>,
    /// Each row's identity: the key columns' RAW fetched values, captured
    /// before display conversion — the WHERE clause binds these.
    identities: Vec<Vec<Value>>,
    /// Identity key -> row index on this page, for projecting staged
    /// changes onto the view.
    row_of: std::collections::HashMap<String, usize>,
    /// Projection of the staged layer onto this page: (row, schema col)
    /// -> staged display text (None = staged NULL).
    staged: std::collections::HashMap<(usize, usize), Option<SharedString>>,
    /// Synthetic INSERT rows prepended to the fetched page, in display
    /// order. Their private keys live only in Edits and never enter SQL.
    draft_keys: Vec<String>,
    /// Explicit draft values. Absence means DEFAULT, distinct from a
    /// present None which means explicit SQL NULL.
    draft_cells: std::collections::HashMap<(usize, usize), Option<SharedString>>,
    /// What an untouched draft cell becomes on commit, per schema column.
    /// Derived once from catalog metadata.
    draft_hints: Vec<DraftHint>,
    /// Each schema column's declared type, the catalog's where it has one
    /// (an ENUM's carries its values), for the column card.
    col_types: Vec<SharedString>,
    /// Rows staged for DELETE — ghosted with strikethrough until commit.
    deleted: std::collections::HashSet<usize>,
    /// The cell whose editor is open, and the editor to render there.
    editing: Option<(usize, usize)>,
    /// The column where the current Tab run began — Sheets' typewriter
    /// anchor. Enter during a run sweeps back to it, one row on; any
    /// arrow, click, or Esc ends the run.
    tab_anchor: Option<usize>,
    editor_input: Option<Entity<gpui_kit::component::input::InputState>>,
    /// Picking an ENUM value on the open editor's column card: fills the
    /// editor and confirms the cell in place, as a dropdown does.
    pick: Option<PickValue>,
    numeric: Vec<bool>,
    /// Schema indices hidden via the Columns popover.
    hidden: std::collections::HashSet<usize>,
    /// Schema indices whose cell text renders as PILLS — " · "-joined
    /// tags in the NULL tag's own chassis (the Structure grid's
    /// attributes column). The grid's one badge vocabulary, extended,
    /// not forked.
    pill_cols: std::collections::HashSet<usize>,
    /// Schema column 0 is the hidden rowid identity — plumbing, not
    /// data: excluded from display, the popover, and every fit.
    identity: bool,
    /// User drag-resizes, schema index → width, reapplied whenever the
    /// column list rebuilds (toggles, refreshes).
    widths: std::collections::HashMap<usize, Pixels>,
    /// Current data-surface zoom, retained so every column rebuild can
    /// reproduce its zoom-aware semantic minimum.
    zoom: f32,
    /// Schema index for each data column currently displayed — the map
    /// render_td/th use, since col_ix stops matching schema order once
    /// anything is hidden.
    visible: Vec<usize>,
    /// Whether the column list currently includes the row-number gutter.
    gutter: bool,
    /// Render-ready cell text (None = NULL), converted once at page
    /// commit — render_td runs per visible cell per frame and must not
    /// allocate, so it only bumps these SharedStrings.
    rows: Vec<Vec<Option<SharedString>>>,
    /// Every selected row, and the one that leads them (docs/EDITING.md
    /// "Selecting rows"). The lead mirrors the table's own selected row
    /// (synced from TableEvent) — the delegate cannot read the TableState
    /// it is rendering inside, so render_tr tints from here.
    selection: RowSelection,
    /// The Sheets corner state: "#" was clicked, every cell highlights,
    /// and the next divider double-click fits the whole table. Any
    /// ordinary cell or row click disarms it.
    all_selected: bool,
    /// The active cell (row, data column), Sheets-style: the clicked cell
    /// carries an accent ring on top of the row tint, and keyboard row
    /// moves carry the ring to the same column of the new row.
    active_cell: Option<(usize, usize)>,
    /// Set while a fetch is in flight; the TableDelegate `loading` hook
    /// reads it, so it lives here rather than on Grid.
    loading: bool,
}

impl Grid {
    /// Last absolute row number currently painted by this grid. The
    /// Query view uses this exact value when synchronizing its top and
    /// bottom row-number rails.
    pub(crate) fn last_visible_row(&self, cx: &App) -> u64 {
        let d = self.table.read(cx).delegate();
        if d.gutter {
            (d.base + d.fetched_count()) as u64
        } else {
            0
        }
    }

    /// Give this grid the shared Query-pane gutter width. This is an
    /// exact assignment, not a grow-only floor: when a later result has
    /// fewer digits, the editor and result grid shrink together.
    pub(crate) fn set_gutter_max(&mut self, max_row: u64, cx: &mut Context<Self>) {
        let want = px(gutter_width(max_row));
        self.table.update(cx, |state, cx| {
            let d = state.delegate_mut();
            if d.gutter && d.cols.first().is_some_and(|col| col.width != want) {
                d.cols[0].width = want;
                state.refresh(cx);
                cx.notify();
            }
        });
    }

    /// Build a grid from an already-fetched first page. The caller fetches
    /// BEFORE constructing (DESIGN.md: fetch first, commit over the old
    /// value), so the swap from the previous grid is one complete frame —
    /// no skeleton, no columns popping in, no gutter re-widening.
    pub(crate) fn new(
        conn: Conn,
        schema: &str,
        name: &str,
        title: String,
        outcome: Result<harbor_client::QueryResult, String>,
        total_rows: Option<u64>,
        // The size the first page was FETCHED with — not re-read from
        // prefs, which may have cycled while the fetch was in flight
        // (a mismatch makes the first next-click skip rows).
        page_size: usize,
        structure: Option<crate::structure::TableStructure>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let source = crate::sql::source(schema, name);
        Self::build(
            conn, source, title, outcome, total_rows, page_size, structure, false,
            true, window, cx,
        )
    }

    /// A results grid for the Query view: the user's statement IS the
    /// source — "a Data window with a custom query preceding it" — and
    /// the same machinery pages `SELECT * FROM (statement)`. No catalog
    /// structure, so no key, so no Edits: read-only by construction,
    /// not by gate.
    pub(crate) fn new_query(
        conn: Conn,
        sql: &str,
        outcome: Result<harbor_client::QueryResult, String>,
        total_rows: Option<u64>,
        page_size: usize,
        pageable: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(
            conn,
            crate::sql::query_source(sql),
            String::new(),
            outcome,
            total_rows,
            page_size,
            None,
            true,
            pageable,
            window,
            cx,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        conn: Conn,
        source: String,
        title: String,
        outcome: Result<harbor_client::QueryResult, String>,
        total_rows: Option<u64>,
        page_size: usize,
        structure: Option<crate::structure::TableStructure>,
        embedded: bool,
        pageable: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let p = prefs::get(cx);
        let gutter = p.row_numbers;
        // Editability follows capability (docs/EDITING.md): a primary key
        // from the catalog when there is one — and DuckDB's implicit
        // rowid when there isn't. Every base table has a rowid, so a
        // keyless table edits like any other: pages fetch the rowid paired
        // with the row's hash (`sql::page_sql`), the column stays hidden,
        // and only the WHERE clauses see it.
        let pk_cols: Vec<String> = structure
            .as_ref()
            .map(|s| s.cols.iter().filter(|c| c.pk).map(|c| c.name.clone()).collect())
            .unwrap_or_default();
        let rowid = structure.as_ref().is_some_and(|s| s.keyed_by_rowid());
        let pk_cols = if rowid { vec!["rowid".to_string()] } else { pk_cols };
        let mut delegate = GridDelegate {
            cols: Vec::new(),
            schema_cols: Vec::new(),
            names: Vec::new(),
            row_labels: Vec::new(),
            base: 0,
            pk_ix: Vec::new(),
            identities: Vec::new(),
            row_of: std::collections::HashMap::new(),
            staged: std::collections::HashMap::new(),
            draft_keys: Vec::new(),
            draft_cells: std::collections::HashMap::new(),
            draft_hints: Vec::new(),
            col_types: Vec::new(),
            deleted: std::collections::HashSet::new(),
            editing: None,
            tab_anchor: None,
            editor_input: None,
            pick: None,
            numeric: Vec::new(),
            hidden: std::collections::HashSet::new(),
            pill_cols: std::collections::HashSet::new(),
            identity: rowid,
            widths: std::collections::HashMap::new(),
            zoom: p.zoom_factor(),
            visible: Vec::new(),
            gutter,
            rows: Vec::new(),
            selection: RowSelection::default(),
            all_selected: false,
            active_cell: None,
            loading: false,
        };
        let (error, last_time_ms) = match outcome {
            Ok(page) => {
                let ms = page.time_ms;
                delegate.commit_schema(page, 0, p.zoom_factor(), &pk_cols);
                (None, ms)
            }
            Err(message) => (Some(message), 0),
        };
        let edits = (!delegate.pk_ix.is_empty()).then(|| {
            Edits::new(
                source.clone(),
                pk_cols.clone(),
                delegate.names.iter().map(|n| n.to_string()).collect(),
                delegate.schema_cols.iter().map(|c| c.duckdb_type.clone()).collect(),
            )
        })
        .map(|e| if rowid { e.keyed_by_rowid() } else { e });
        let (not_null, defaults, generated, hints, types) =
            insert_metadata(&delegate.names, structure.as_ref());
        delegate.draft_hints = hints;
        delegate.col_types = col_types(types, &delegate.schema_cols);
        // Header dragging stays off until move_column permutes the
        // visible map for real — the library default half-enables it
        // (widths reorder, contents don't).
        // Column selection stays off too: a header click would switch
        // the Table into column mode, where it reports no selected row
        // and the observer below would clear the grid's selection.
        let table = cx.new(|cx| {
            TableState::new(delegate, window, cx)
                .col_movable(false)
                .col_selectable(false)
                // ⌘- and ⇧-clicks belong to the grid's own multi-row
                // selection, decided on mouse down; the table re-selecting
                // the row on the click would undo a ⌘-click that removed it.
                .select_on_modifier_click(false)
        });
        // The table binds plain up/down/left/right/escape to its own
        // selection actions, and gpui dispatches BINDINGS before raw key
        // listeners — so the wash moved by the table's action and the
        // ring moved by our listener-fed mirror arrived as two writes,
        // a perceptible lump-lump. This interceptor runs before binding
        // dispatch, and a stopped event skips bindings AND listeners
        // both — so for the five keys we own, in navigation, with our
        // table focused, it performs the move itself: ring and wash in
        // ONE mutation, nobody else consulted. Single writer, atomic
        // paint.
        let weak = cx.entity().downgrade();
        let intercept = cx.intercept_keystrokes(move |ev, window, cx| {
            let ks = &ev.keystroke;
            if ks.modifiers != Modifiers::default() {
                return; // modified chords stay on the normal paths
            }
            if !matches!(ks.key.as_str(), "up" | "down" | "left" | "right" | "escape") {
                return;
            }
            let Some(grid) = weak.upgrade() else { return };
            {
                let g = grid.read(cx);
                if g.editor.is_some() {
                    return; // the open editor's own keys keep their meanings
                }
                if !g.table.focus_handle(cx).contains_focused(window, cx) {
                    return;
                }
            }
            let key = ks.key.clone();
            grid.update(cx, |g, cx| {
                // A bare arrow with no ring yet just takes the top-left
                // seat (Steve's ruling): the first press shows you where
                // you are, the next one moves. seed_ring says whether it
                // placed the ring; a placed ring consumes the press.
                let seeded =
                    key != "escape" && g.seed_ring(cx);
                if seeded {
                    return;
                }
                match key.as_str() {
                    "up" => g.move_ring(-1, 0, cx),
                    "down" => g.move_ring(1, 0, cx),
                    "left" => g.move_ring(0, -1, cx),
                    "right" => g.move_ring(0, 1, cx),
                    _ => g.escape_ring(cx),
                }
            });
            cx.stop_propagation();
        });
        cx.subscribe(&table, |_, table, event: &gpui_kit::component::table::TableEvent, cx| {
            match event {
                gpui_kit::component::table::TableEvent::SelectRow(ix) => {
                    let ix = *ix;
                    table.update(cx, |state, cx| {
                        let d = state.delegate_mut();
                        d.selection.lead_on(Some(ix));
                        // Keyboard row moves carry the active-cell ring to
                        // the same column of the new row (Sheets' arrow
                        // behavior).
                        if let Some((_, col)) = d.active_cell {
                            d.active_cell = Some((ix, col));
                        }
                        cx.notify();
                    });
                    cx.notify();
                }
                gpui_kit::component::table::TableEvent::ColumnWidthsChanged(widths) => {
                    // Mirror drag-resizes into the delegate, keyed by
                    // schema column — otherwise any refresh rebuilds the
                    // layout from the delegate's original widths and the
                    // user's resize snaps back.
                    let widths = widths.clone();
                    table.update(cx, |state, cx| {
                        let d = state.delegate_mut();
                        let g = d.gutter as usize;
                        // With the corner's select-all armed, dragging ONE
                        // divider sizes every column to it, Sheets-style:
                        // the dragged column is the one whose width moved.
                        if d.all_selected {
                            let dragged = widths.iter().enumerate().skip(g).find_map(|(i, w)| {
                                (d.cols.get(i).map(|c| c.width) != Some(*w)).then_some(*w)
                            });
                            if let Some(w) = dragged {
                                for schema_ix in d.visible.clone() {
                                    d.widths.insert(schema_ix, w);
                                }
                                d.rebuild_cols();
                                state.refresh(cx);
                                return;
                            }
                        }
                        let d = state.delegate_mut();
                        for (i, w) in widths.iter().enumerate() {
                            if let Some(col) = d.cols.get_mut(i) {
                                col.width = *w;
                            }
                            if i >= g {
                                if let Some(&schema_ix) = d.visible.get(i - g) {
                                    d.widths.insert(schema_ix, *w);
                                }
                            }
                        }
                    });
                }
                _ => {}
            }
        })
        .detach();
        // SelectRow alone would let the mirror drift — ghost tint and
        // ring on a row the table considers deselected — since the Table
        // also clears and moves its selection on its own (Escape, a
        // header click). Reconcile on every table notify instead; the
        // comparison makes it a no-op when already in sync.
        cx.observe(&table, |_, table, cx| {
            table.update(cx, |state, cx| {
                let real = state.selected_row();
                let d = state.delegate_mut();
                // Dirty rows deliberately suppress the Table's blue row
                // selection so their amber/red staging wash has one owner.
                // That intentional `real = None` is not an Escape and must
                // not erase the active-cell ring (especially between a
                // Tab-confirm and opening its destination editor).
                let dirty_mirror = d.selection.lead.is_some_and(|row| d.row_dirty(row));
                if should_reconcile_selection(real, d.selection.lead, dirty_mirror) {
                    d.selection.lead_on(real);
                    if real.is_none() {
                        d.active_cell = None;
                    }
                    cx.notify();
                }
            });
        })
        .detach();
        let resize = cx.new(|_| ResizableState::default());
        cx.subscribe(&resize, |_, state, _: &ResizablePanelEvent, cx| {
            if let Some(width) = state.read(cx).sizes().get(1).copied() {
                crate::prefs::save(cx, |p| {
                    p.inspector_width = f32::from(width)
                        .clamp(prefs::INSPECTOR_MIN, prefs::INSPECTOR_MAX);
                });
            }
        })
        .detach();
        let col_search = cx.new(|cx| {
            gpui_kit::component::input::InputState::new(window, cx)
                .placeholder("Search columns\u{2026}")
        });
        let ddl = structure.as_ref().and_then(|s| s.ddl.clone());
        let ddl_copy = ddl
            .clone()
            .map(|ddl| cx.new(|_| crate::copy_button::CopyButton::new("Copy DDL", ddl)));
        let ddl_input = ddl.map(|ddl| {
            // A code editor (language "duckdb"), so the DDL wears the
            // same tree-sitter highlighting as the Query view — but
            // numberless and unfolded, with its height pinned at render
            // time (structure.rs). The card is content-sized and never
            // scrolls: no room past the last line, and the caret never
            // pulls the text toward a margin.
            cx.new(|cx| {
                gpui_kit::component::input::EditorState::new(window, cx)
                    .language("duckdb")
                    .line_number(false)
                    .folding(false)
                    .scroll_beyond_last_line(Some(0))
                    .cursor_surrounding_lines(Some(0))
                    .default_value(ddl)
            })
        });
        cx.subscribe(&col_search, |_, _, _: &gpui_kit::component::input::InputEvent, cx| {
            cx.notify();
        })
        .detach();
        // The DDL editor re-wraps when it learns its real width — first
        // paint, pane resize, zoom — and notifies ITSELF. The card's
        // height is OUR render's math (wrap_row_count), so observe
        // the editor: the card resizes in the very next frame instead
        // of waiting for an incidental repaint (the first-display chop).
        if let Some(input) = &ddl_input {
            cx.observe(input, |_, _, cx| cx.notify()).detach();
        }
        let structure_grid = structure
            .as_ref()
            .map(|s| crate::structure::columns_grid(conn.clone(), s, window, cx));
        // The columns/DDL divider persists like every other divider
        // (sidebar, inspector, query): only the user's drag writes it.
        let structure_split = (structure_grid.is_some() && ddl_input.is_some()).then(|| {
            let state = cx.new(|_| ResizableState::default());
            cx.subscribe(
                &state,
                |_, state, _: &gpui_kit::component::resizable::ResizablePanelEvent, cx| {
                    if let Some(h) = state.read(cx).sizes().first().copied() {
                        crate::prefs::save(cx, |p| {
                            p.structure_split = f32::from(h)
                                .clamp(prefs::STRUCTURE_SPLIT_MIN, prefs::STRUCTURE_SPLIT_MAX);
                        });
                    }
                },
            )
            .detach();
            state
        });
        Self {
            table,
            conn,
            session: None,
            query_view: None,
            query_obs: None,
            embedded,
            pageable,
            source,
            title,
            page: 0,
            page_size,
            filter: None,
            total_rows,
            error,
            last_time_ms,
            edits,
            editor: None,
            rowid,
            pk_cols,
            not_null,
            defaults,
            generated,
            parked: None,
            reshaped: false,
            committing: false,
            post_commit: None,
            unrefreshed: false,
            commit_session: Default::default(),
            needs_focus: false,
            ring_keep: None,
            header_chase: false,
            _intercept: intercept,
            filter_input: None,
            filter_esc: None,
            structure,
            structure_grid,
            structure_split,
            resize,
            col_search,
            ddl_input,
            ddl_copy,
            fetch_seq: 0,
            table_bounds: std::rc::Rc::new(std::cell::Cell::new(Bounds::default())),
        }
    }

    /// Fetch a page (and optionally a fresh count) in the background and
    /// commit everything — rows, page, size, filter — in one frame. The
    /// current page stays on screen until then; an error keeps it and
    /// shows in the strip. A newer fetch supersedes an older one in
    /// flight (the fence below), so rapid clicks converge on the latest
    /// request instead of dropping it.
    fn fetch(&mut self, req: PageReq, cx: &mut Context<Self>) {
        self.fetch_seq += 1;
        let fence = self.fetch_seq;
        let filter = match &req.filter {
            FilterChange::Set(new) => new.clone(),
            FilterChange::Keep => self.filter.clone(),
        };
        let conn = self.conn.clone();
        let session = self.session.clone();
        let sql =
            crate::sql::page_sql(&self.source, self.rowid, &filter, req.page, req.size);
        // The fetch a commit is waiting on counts again, since the commit
        // changed the count, and so does any fetch that supersedes it: a
        // page flipped in that window must not leave the total as it was.
        let recount = req.recount || self.post_commit.is_some();
        let count_sql = recount.then(|| crate::sql::count_sql(&self.source, &filter));
        let PageReq { page, size, filter, .. } = req;
        // A held set whose commit was not seen to end: its session is asked
        // after again before this page is read, so a page that arrives with
        // the session gone was read after the commit was over.
        let unsettled = self.edits.as_ref().and_then(|e| e.unsettled().map(str::to_string));
        self.table.update(cx, |state, _| state.delegate_mut().loading = true);
        cx.spawn(async move |this, cx| {
            let (outcome, over) = cx
                .background_executor()
                .spawn(async move {
                    let over = unsettled.map(|id| {
                        harbor_client::session_end(&conn, &id, std::time::Duration::from_secs(2))
                            == harbor_client::Ended::Settled
                    });
                    let run = |sql: &str| match &session {
                        Some(txn) => txn.page(sql),
                        None => harbor_client::query(&conn, sql),
                    };
                    let outcome = run(&sql).map(|result| {
                        let total =
                            count_sql.map(|c| run(&c).ok().and_then(|r| crate::sql::count_of(&r)));
                        (result, total)
                    });
                    (outcome, over)
                })
                .await;
            this.update(cx, |grid, cx| {
                // Superseded by a newer fetch: this outcome is stale and
                // commits nothing (the newer fetch owns the loading flag).
                if grid.fetch_seq != fence {
                    return;
                }
                // The page's columns against the grid's: another client may
                // have altered the table since this grid took its schema.
                let reshaped = outcome.as_ref().is_ok_and(|(result, _)| {
                    let d = grid.table.read(cx).delegate();
                    !d.schema_cols.is_empty() && !same_columns(&d.schema_cols, &result.columns)
                });
                // The page query answered, whether its page is kept or
                // dropped below: the database was read at this moment.
                let read = outcome.is_ok();
                let result = match outcome {
                    // Edits staged against the columns on screen are keyed
                    // and typed by them. They are not rebound to the table's
                    // present shape, and its rows do not fit the view they
                    // live in: the page is dropped and the view stays.
                    Ok(_) if reshaped && grid.edits.as_ref().is_some_and(Edits::any_staged) => {
                        grid.reshaped = true;
                        grid.error = stale_reason(true, false, grid.in_doubt()).map(str::to_string);
                        None
                    }
                    Ok((result, total)) => {
                        grid.reshaped = false;
                        grid.unrefreshed = false;
                        grid.error = None;
                        grid.page = page;
                        grid.page_size = size;
                        if let FilterChange::Set(f) = filter {
                            grid.filter = f;
                        }
                        if let Some(t) = total {
                            grid.total_rows = t;
                        }
                        grid.last_time_ms = result.time_ms;
                        // New rows displace old indexes; an editor left
                        // open would be typing into a stranger's cell.
                        grid.editor = None;
                        Some(result)
                    }
                    Err(message) => {
                        grid.error = Some(message);
                        None
                    }
                };
                let base = page * size;
                let zoom = prefs::get(cx).zoom_factor();
                let pk_cols = grid.pk_cols.clone();
                let fetched = result.is_some();
                // Taken unconditionally: a failed flip must not park a
                // stale seat for some later, unrelated fetch to restore.
                let ring_keep = grid.ring_keep.take();
                let mut born = false;
                grid.table.update(cx, |state, cx| {
                    state.delegate_mut().loading = false;
                    if let Some(result) = result {
                        {
                            let d = state.delegate_mut();
                            if d.schema_cols.is_empty() || reshaped {
                                // An error-born grid (first page failed)
                                // has no schema yet, and a reshaped table's
                                // is not this one: adopt the page's — the
                                // same birth Grid::new gives a healthy
                                // first page.
                                d.commit_schema(result, base, zoom, &pk_cols);
                                born = true;
                            } else {
                                d.adopt_rows(result.rows, base);
                            }
                            d.selection.clear();
                            d.active_cell = None;
                            d.editing = None;
                            d.editor_input = None;
                            d.pick = None;
                            d.tab_anchor = None;
                        }
                        let d = state.delegate();
                        if d.gutter {
                            let last = (base + d.rows.len()) as u64;
                            let want = px(gutter_width(last));
                            if d.cols[0].width != want {
                                state.delegate_mut().cols[0].width = want;
                                state.refresh(cx);
                            }
                        }
                        state.clear_selection(cx);
                        if !state.delegate().rows.is_empty() {
                            state.scroll_to_row(0, cx);
                        }
                    }
                    cx.notify();
                });
                // A keyboard page flip keeps the ring seated: same
                // column (if still visible), same row clamped to the new
                // page's rows.
                if let (Some((r, c)), true) = (ring_keep, grid.error.is_none()) {
                    grid.table.update(cx, |state, cx| {
                        let d = state.delegate_mut();
                        if d.rows.is_empty() {
                            return;
                        }
                        let row = r.min(d.rows.len() - 1);
                        let col = if d.visible.contains(&c) {
                            c
                        } else {
                            d.visible.first().copied().unwrap_or(0)
                        };
                        d.active_cell = Some((row, col));
                        select_row(state, row, cx);
                        cx.notify();
                    });
                }
                // A schema just landed: everything build() derives from one
                // is derived again, from this one.
                if born {
                    grid.settle_schema(cx);
                }
                if reshaped && born {
                    // The catalog the sidebar and the next grid read from
                    // describes the table as it was.
                    cx.emit(crate::app::CatalogRefreshRequested);
                }
                // Staged changes are identity-keyed; the new page gets
                // them projected wherever (and whether) its rows match.
                // A projection only: a fetch never answers itself with
                // another (`sync_staged`).
                // The fetch a commit was waiting on: fenced like any other,
                // so this is the newest page, fetched after the commit. What
                // the commit left is settled before the projection, which
                // depends on it: a set whose commit got no answer is held
                // and not drawn, and a page that did not arrive after a
                // commit that landed keeps what the commit folded into it.
                let after = grid.post_commit.take();
                // The database was read with a held set waiting on that: if
                // the commit is over by this read, the set can be judged. A
                // page dropped because the table has other columns counts
                // too. It cannot be shown, and a set held against the
                // columns it has not is only ever dropped, which needs no
                // page; left out, such a set could never be judged at all.
                let judgeable = read
                    && after.is_none()
                    && grid.edits.as_mut().is_some_and(|edits| {
                        let waiting = edits.unjudged().is_some();
                        edits.fetched(over);
                        waiting && edits.unjudged().is_none()
                    });
                if judgeable {
                    grid.error = Some(if grid.reshaped {
                        HELD_RESHAPED.to_string()
                    } else {
                        format!("the commit is over, and this page was read after it \u{b7} {HELD}")
                    });
                }
                if settle_fetch(after.as_ref(), read, grid.edits.as_mut()) {
                    grid.unrefreshed = true;
                }
                grid.project_staged(cx);
                if let Some(after) = after {
                    grid.settle_commit(after, fetched, read, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The page fetch after a commit has landed or failed: staging opens
    /// again, and the status line says what the user needs to know. A page
    /// that did not arrive leaves the rows of before the commit on screen,
    /// so nothing is staged against them until one does (`unrefreshed`).
    /// `fetched` is whether a page arrived and is on screen, `read` whether
    /// the page query answered: a page read and dropped, because the table
    /// has other columns, is neither a failure nor a page to look at.
    fn settle_commit(&mut self, after: PostCommit, fetched: bool, read: bool, cx: &mut Context<Self>) {
        self.committing = false;
        self.unrefreshed = !fetched && after == PostCommit::Landed;
        let failure = if read { None } else { self.error.take() };
        let page = match (fetched, read) {
            (true, _) => Page::Read,
            (false, true) => Page::Reshaped,
            (false, false) => Page::Failed(failure.as_deref().unwrap_or("no answer")),
        };
        self.error = commit_status(&after, page);
        cx.emit(crate::app::CommitSettled);
    }

    /// What `build` derives from a schema, derived from the one a fetch just
    /// committed: the staging layer, when the schema is fully keyed, and the
    /// per-column insert metadata. Nothing is staged when this runs — an
    /// error-born grid has no staging layer yet, and a reshaped table is
    /// adopted only with none — so the set built here is empty, and the
    /// stash parked for this table is judged against it.
    fn settle_schema(&mut self, cx: &mut Context<Self>) {
        let (keyed, names, types) = {
            let d = self.table.read(cx).delegate();
            (
                !d.pk_ix.is_empty(),
                d.names.clone(),
                d.schema_cols.iter().map(|c| c.duckdb_type.clone()).collect::<Vec<_>>(),
            )
        };
        self.edits = keyed.then(|| {
            let edits = Edits::new(
                self.source.clone(),
                self.pk_cols.clone(),
                names.iter().map(|n| n.to_string()).collect(),
                types,
            );
            if self.rowid { edits.keyed_by_rowid() } else { edits }
        });
        let (not_null, defaults, generated, hints, types) =
            insert_metadata(&names, self.structure.as_ref());
        self.not_null = not_null;
        self.defaults = defaults;
        self.generated = generated;
        self.table.update(cx, |state, cx| {
            let d = state.delegate_mut();
            d.draft_hints = hints;
            d.col_types = col_types(types, &d.schema_cols);
            d.rebuild_cols();
            state.refresh(cx);
        });
        if let Some(stash) = self.parked.take() {
            self.adopt_edits(stash, cx);
        }
    }

    /// While the table has other columns than the ones on screen, the page
    /// on screen predates a commit that landed, or the staged set is held
    /// because its commit got no answer, every gesture that stages or
    /// commits is refused with the reason. Discard stays open throughout.
    /// The way out of the first is to discard or undo, of the second a
    /// refresh, of the third the review popover.
    fn refuse_stale(&mut self, cx: &mut Context<Self>) -> bool {
        let reason = stale_reason(self.reshaped, self.unrefreshed, self.in_doubt());
        if let Some(reason) = reason {
            self.error = Some(reason.to_string());
            cx.notify();
        }
        reason.is_some()
    }

    /// A gesture that replaces the page, or the grid itself, is about to
    /// run. Text in an open editor is staged first, as a refresh does, so
    /// the gesture does not cancel typing; false when the text was refused
    /// and the editor stays open with the reason, the page where it was.
    pub(crate) fn settle_editor(&mut self, cx: &mut Context<Self>) -> bool {
        self.editor.is_none() || self.confirm_and_move(0, 0, cx)
    }

    /// Navigate to a page at the current size and filter.
    fn fetch_page(&mut self, page: usize, cx: &mut Context<Self>) {
        if !self.settle_editor(cx) {
            return;
        }
        let size = self.page_size;
        self.fetch(PageReq { page, size, filter: FilterChange::Keep, recount: false }, cx);
    }

    pub(crate) fn jump_first(&mut self, cx: &mut Context<Self>) {
        if self.page > 0 {
            self.fetch_page(0, cx);
        }
    }

    /// Jump to the last page — only reachable once the total is known,
    /// because the offset comes from it.
    pub(crate) fn jump_last(&mut self, cx: &mut Context<Self>) {
        if let Some(last) = self.last_page() {
            if self.page < last {
                self.fetch_page(last, cx);
            }
        }
    }

    pub(crate) fn prev_page(&mut self, cx: &mut Context<Self>) {
        if self.page > 0 {
            self.fetch_page(self.page - 1, cx);
        }
    }

    pub(crate) fn next_page(&mut self, cx: &mut Context<Self>) {
        if self.has_next(cx) {
            self.fetch_page(self.page + 1, cx);
        }
    }

    /// Last page index under the current count, when known.
    pub(crate) fn last_page(&self) -> Option<usize> {
        let total = self.total_rows?;
        Some((total.max(1) as usize - 1) / self.page_size)
    }

    /// Whether a next page plausibly exists. Unknown total: a full page
    /// suggests there may be more.
    pub(crate) fn has_next(&self, cx: &App) -> bool {
        match self.last_page() {
            Some(last) => self.page < last,
            None => self.table.read(cx).delegate().fetched_count() == self.page_size,
        }
    }

    /// Cycle the page size through PAGE_SIZES (a global preference) and
    /// refetch from page 1. The pref cycles immediately (so rapid clicks
    /// advance through the sizes), but the delegate's size commits with
    /// the rows fetched at it — the footer never labels old rows with a
    /// new size.
    pub(crate) fn cycle_page_size(&mut self, cx: &mut Context<Self>) {
        if !self.settle_editor(cx) {
            return;
        }
        let current = prefs::get(cx).page_size;
        let ix = prefs::PAGE_SIZES.iter().position(|s| *s == current).unwrap_or(0);
        let next = prefs::PAGE_SIZES[(ix + 1) % prefs::PAGE_SIZES.len()];
        prefs::toggle(cx, |p| p.page_size = next);
        self.fetch(PageReq { page: 0, size: next, filter: FilterChange::Keep, recount: false }, cx);
    }

    /// Mark a schema column as pill-rendered (see GridDelegate::pill_cols).
    pub(crate) fn mark_pill_column(&mut self, schema_ix: usize, cx: &mut Context<Self>) {
        self.table.update(cx, |state, _| {
            state.delegate_mut().pill_cols.insert(schema_ix);
        });
    }

    /// The grid the footer's stats and pager describe when the Query
    /// view is up: the query view's embedded results grid.
    pub(crate) fn query_results_grid(&self, cx: &App) -> Option<Entity<Grid>> {
        self.query_view.as_ref().and_then(|q| q.read(cx).results_grid())
    }

    /// Route a pager action to the grid it belongs to: this one (Data),
    /// or the Query view's results grid. Both are Grids — the whole
    /// point of the unification — so one closure fits either.
    pub(crate) fn pager_dispatch(
        &mut self,
        cx: &mut Context<Self>,
        act: impl FnOnce(&mut Grid, &mut Context<Grid>),
    ) {
        if prefs::get(cx).view == ViewMode::Query {
            if let Some(grid) = self.query_results_grid(cx) {
                grid.update(cx, act);
            }
            return;
        }
        act(self, cx)
    }

    /// Open or close the raw-SQL filter strip. Closing clears an active
    /// filter (refetching unfiltered).
    pub(crate) fn toggle_filter_strip(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.filter_input.is_some() {
            // Closing the strip drops its filter, which replaces the page:
            // text in an open editor is settled first, and text the column
            // refuses keeps the strip, the filter and the page as they are.
            if self.filter.is_some() && !self.settle_editor(cx) {
                return;
            }
            self.filter_input = None;
            self.filter_esc = None;
            if self.filter.is_some() {
                let size = self.page_size;
                self.fetch(
                    PageReq { page: 0, size, filter: FilterChange::Set(None), recount: true },
                    cx,
                );
            }
            cx.notify();
            return;
        }
        let input = cx.new(|cx| {
            gpui_kit::component::input::InputState::new(window, cx)
                .placeholder("e.g. price > 100 AND name LIKE '%panel%'")
        });
        cx.subscribe(&input, |grid, input, event: &gpui_kit::component::input::InputEvent, cx| {
            if matches!(event, gpui_kit::component::input::InputEvent::PressEnter { .. }) {
                if !grid.settle_editor(cx) {
                    return;
                }
                let text = input.read(cx).value().trim().to_string();
                let size = grid.page_size;
                grid.fetch(
                    PageReq {
                        page: 0,
                        size,
                        filter: FilterChange::Set((!text.is_empty()).then_some(text)),
                        recount: true,
                    },
                    cx,
                );
            }
        })
        .detach();
        input.update(cx, |state, cx| state.focus(window, cx));
        // Escape, the sidebar filters' grammar: text present -> clear
        // it (and drop an APPLIED filter with it, so an empty box
        // never sits over secretly-filtered rows); empty -> dismiss
        // the strip.
        let weak = cx.entity().downgrade();
        let weak_input = input.downgrade();
        self.filter_esc = Some(cx.intercept_keystrokes(move |ev, window, cx| {
            if ev.keystroke.key != "escape" {
                return;
            }
            let (Some(grid), Some(input)) = (weak.upgrade(), weak_input.upgrade())
            else {
                return;
            };
            if !input.read(cx).focus_handle(cx).is_focused(window) {
                return;
            }
            if input.read(cx).value().is_empty() {
                grid.update(cx, |grid, cx| grid.toggle_filter_strip(window, cx));
            } else {
                input.update(cx, |state, cx| state.set_value("", window, cx));
                grid.update(cx, |grid, cx| {
                    if grid.filter.is_some() && grid.settle_editor(cx) {
                        let size = grid.page_size;
                        grid.fetch(
                            PageReq {
                                page: 0,
                                size,
                                filter: FilterChange::Set(None),
                                recount: true,
                            },
                            cx,
                        );
                    }
                });
            }
            cx.stop_propagation();
        }));
        self.filter_input = Some(input);
        cx.notify();
    }

    /// Shared tail of every column-set mutation (returns false = no
    /// change): rebuild the display columns, refresh the table, and
    /// reset the horizontal scroll — the header (overflow_scroll) and
    /// body (virtual_list) share a scroll handle but clamp a stale
    /// offset differently once the column set changes width, so origin
    /// is the one offset they agree on.
    fn remap_columns(
        &mut self,
        cx: &mut Context<Self>,
        mutate: impl FnOnce(&mut GridDelegate) -> bool,
    ) {
        self.table.update(cx, |state, cx| {
            if !mutate(state.delegate_mut()) {
                return;
            }
            state.delegate_mut().rebuild_cols();
            state.refresh(cx);
            state.scroll_to_col(0, cx);
            cx.notify();
        });
        cx.notify();
    }

    /// Show or hide one column (never the last visible one).
    pub(crate) fn toggle_column(&mut self, schema_ix: usize, cx: &mut Context<Self>) {
        self.remap_columns(cx, |d| {
            if !d.hidden.remove(&schema_ix) {
                if d.visible.len() <= 1 {
                    return false;
                }
                d.hidden.insert(schema_ix);
            }
            true
        });
    }

    /// The Sheets divider gesture, resolved geometrically: a double-click
    /// in the header row within 4px of a column's right boundary fits that
    /// column — or, with the corner's select-all armed, every column. The
    /// boundary positions come from the delegate's own widths plus the
    /// horizontal scroll offset, so the gesture works at any scroll and on
    /// the divider line itself.
    fn divider_double_click(
        &mut self,
        e: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if e.click_count != 2 {
            return;
        }
        let bounds = self.table_bounds.get();
        let zoom = prefs::get(cx).zoom_factor();
        let header_h = prefs::get(cx).table_size().table_row_height();
        self.table.update(cx, |state, cx| {
            let y = e.position.y - bounds.origin.y;
            if y < px(0.) || y > header_h {
                return; // the gesture lives in the header row, as in Sheets
            }
            let scroll_x = state.horizontal_scroll_handle.base_handle().offset().x;
            let x = e.position.x - bounds.origin.x - scroll_x;
            let d = state.delegate_mut();
            let g = d.gutter as usize;
            let mut cum = px(0.);
            for (disp, col) in d.cols.iter().enumerate() {
                cum += col.width;
                // The gutter/first-column boundary is not a data divider.
                if disp < g {
                    continue;
                }
                if (x - cum).abs() <= px(4.) {
                    let Some(&schema_ix) = d.visible.get(disp - g) else { return };
                    if d.all_selected {
                        d.all_selected = false;
                        d.fit_widths(zoom);
                    } else {
                        d.fit_one(schema_ix, zoom);
                    }
                    state.refresh(cx);
                    cx.notify();
                    return;
                }
            }
        });
    }

    /// Re-fit every column to the page on screen (View menu / Cmd-Shift-F)
    /// — the manual Sheets move, for after drags or a page whose content
    /// outgrew the first page's fit.
    pub(crate) fn fit_columns(&mut self, cx: &mut Context<Self>) {
        let zoom = prefs::get(cx).zoom_factor();
        self.table.update(cx, |state, cx| {
            state.delegate_mut().fit_widths(zoom);
            state.refresh(cx);
        });
        cx.notify();
    }

    /// Reset every hidden column (the popover's "Show all").
    pub(crate) fn show_all_columns(&mut self, cx: &mut Context<Self>) {
        self.remap_columns(cx, |d| {
            if d.hidden.is_empty() {
                return false;
            }
            d.hidden.clear();
            true
        });
    }

    /// Hide every column but the first visible one (the popover's "Hide
    /// all" — the grid never goes to zero columns, so start-from-nothing
    /// keeps one anchor to build from).
    pub(crate) fn hide_all_columns(&mut self, cx: &mut Context<Self>) {
        self.remap_columns(cx, |d| {
            if d.visible.len() <= 1 {
                return false;
            }
            let keep = d.visible[0];
            d.hidden = (0..d.schema_cols.len()).filter(|i| *i != keep).collect();
            true
        });
    }

    /// (schema index, name, hidden) for the Columns popover.
    pub(crate) fn column_list(&self, cx: &App) -> Vec<(usize, SharedString, bool)> {
        let d = self.table.read(cx).delegate();
        d.names
            .iter()
            .enumerate()
            .skip(d.identity as usize)
            .map(|(i, name)| (i, name.clone(), d.hidden.contains(&i)))
            .collect()
    }

    pub(crate) fn structure(&self) -> Option<&crate::structure::TableStructure> {
        self.structure.as_ref()
    }

    /// The row and (visible, gutterless) column counts, plus whether a
    /// fetch is in flight — the delegate's side of the footer's status
    /// line (footer.rs; Grid's side reads straight off the fields).
    pub(crate) fn table_facts(&self, cx: &App) -> (usize, usize, bool) {
        let d = self.table.read(cx).delegate();
        (d.fetched_count(), d.cols.len().saturating_sub(d.gutter as usize), d.loading)
    }

    /// The selected row as (column, display value, is_null) pairs, for the
    /// inspector's ROW section. SharedStrings all the way — this runs on
    /// every notify with the inspector open, so it only bumps refcounts.
    /// Reads the delegate's own selection mirror, the one source render_tr
    /// tints from.
    pub(crate) fn row_kv(&self, cx: &App) -> Option<Vec<(SharedString, SharedString, bool)>> {
        let d = self.table.read(cx).delegate();
        let row_ix = d.selection.lead?;
        let row = d.rows.get(row_ix)?;
        let is_draft = d.draft_key(row_ix).is_some();
        Some(
            d.names
                .iter()
                .enumerate()
                .skip(d.identity as usize)
                .map(|(i, name)| {
                    if is_draft && !d.draft_cells.contains_key(&(row_ix, i)) {
                        let hint = d.draft_hints.get(i).cloned().unwrap_or(DraftHint::Null);
                        return (name.clone(), hint.describe(), true);
                    }
                    match row.get(i) {
                        None | Some(None) => (name.clone(), SharedString::from("NULL"), true),
                        Some(Some(s)) => (name.clone(), s.clone(), false),
                    }
                })
                .collect(),
        )
    }

    // ------------------------------------------------------------------
    // Editing (docs/EDITING.md). One meaning per key; Esc is lossless;
    // nothing writes until ⌘S.
    // ------------------------------------------------------------------

    /// Add an intentional all-DEFAULT draft at the top of the current
    /// page, then enter its first useful writable cell. The draft is an
    /// ordinary staged change immediately; moving focus never writes it.
    pub(crate) fn add_row(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.committing || self.edits.is_none() || self.refuse_stale(cx) {
            return;
        }
        if self.editor.is_some() && !self.confirm_and_move(0, 0, cx) {
            return;
        }
        let key = self.edits.as_mut().expect("checked above").stage_insert();
        self.error = None;
        self.sync_staged(cx);
        self.focus_draft(&key, window, cx);
    }

    /// Stage a new INSERT copied from the selected fetched row. Generated
    /// columns and primary-key columns are omitted so DuckDB can compute
    /// them; a natural key without a default therefore appears REQUIRED.
    /// The INSERT reads each cell from the source row in SQL; a cell with a
    /// staged update, which the database does not hold, is bound instead.
    pub(crate) fn duplicate_row(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A read-only table and a commit in flight say so in the footer,
        // and the Edit menu's gate (`accepts_row_commands`) stops both first.
        if self.committing || self.edits.is_none() || self.refuse_stale(cx) {
            return;
        }
        if self.editor.is_some() && !self.confirm_and_move(0, 0, cx) {
            return;
        }
        let source = {
            let d = self.table.read(cx).delegate();
            let row = d.selection.lead.or(d.active_cell.map(|(row, _)| row));
            let lead = row.map(|row| {
                if d.draft_key(row).is_some() {
                    Lead::Draft
                } else if d.deleted.contains(&row) {
                    Lead::Deleted
                } else {
                    Lead::Persisted
                }
            });
            match duplicate_refusal(lead) {
                Some(reason) => Err(reason),
                None => Ok(row.and_then(|row| {
                    Some((
                        d.identities.get(row).cloned()?,
                        d.rows.get(row).cloned()?,
                        d.identity as usize,
                        d.pk_ix.clone(),
                    ))
                })),
            }
        };
        let (identity, fetched, first_schema, pk_ix) = match source {
            Ok(Some(source)) => source,
            // A persisted row always has its cells and its identity.
            Ok(None) => return,
            Err(reason) => {
                self.error = Some(reason.to_string());
                cx.notify();
                return;
            }
        };

        let source_key = edits::key_of(&identity);
        let edits = self.edits.as_mut().expect("checked above");
        let staged = edits
            .entries()
            .into_iter()
            .find(|(key, _, _)| *key == source_key)
            .and_then(|(_, _, change)| match change {
                edits::RowChange::Update(cells) => Some(cells.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let cells = duplicate_cells(fetched, &staged, first_schema, &pk_ix, &self.generated);
        let key = edits.stage_duplicate(identity, cells);
        self.error = None;
        self.sync_staged(cx);
        self.focus_draft(&key, window, cx);
    }

    /// Select a staged INSERT and enter the first field that needs or can
    /// accept input. Shared by New Row and Duplicate Row.
    fn focus_draft(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        // sync_staged normally returns focus after a draft row appears or
        // disappears; row creation is the exception because its editor is
        // the intended destination.
        self.needs_focus = false;
        let (row, col, reveal) = {
            let d = self.table.read(cx).delegate();
            let row = d.draft_keys.iter().position(|k| k == key).unwrap_or(0);
            let first_schema = d.identity as usize;
            let required = (first_schema..d.names.len()).find(|&col| {
                self.not_null.get(col).copied().unwrap_or(false)
                    && self.defaults.get(col).and_then(Option::as_ref).is_none()
                    && !self.generated.get(col).copied().unwrap_or(false)
            });
            let fallback = d
                .visible
                .iter()
                .copied()
                .find(|&col| !self.generated.get(col).copied().unwrap_or(false));
            let col = required.or(fallback);
            (row, col, col.is_some_and(|col| d.hidden.contains(&col)))
        };
        if reveal {
            self.remap_columns(cx, |d| d.hidden.remove(&col.expect("reveal has a column")));
        }
        self.table.update(cx, |state, cx| {
            if let Some(col) = col {
                state.delegate_mut().active_cell = Some((row, col));
            }
            select_row(state, row, cx);
            state.scroll_to_row(row, cx);
            cx.notify();
        });
        if let Some(col) = col {
            self.open_editor(row, col, None, window, cx);
        } else {
            // A generated-only table has no writable destination for an
            // editor, but the staged DEFAULT row still needs keyboard
            // focus so it can be committed or discarded.
            self.needs_focus = true;
        }
        cx.notify();
    }

    /// Native Edit-menu actions arrive at App scope, so they need one
    /// explicit gate before reaching the grid. Row commands belong only
    /// to the focused Data table, never to text inputs or embedded grids.
    pub(crate) fn accepts_row_commands(&self, window: &Window, cx: &App) -> bool {
        !self.embedded
            && prefs::get(cx).view == ViewMode::Data
            && self.editor.is_none()
            && self.edits.is_some()
            && !self.committing
            && self.table.focus_handle(cx).contains_focused(window, cx)
    }

    /// The grid's whole keymap, focus-scoped by construction: this
    /// listener sits on the grid wrapper, so it hears keys only when
    /// focus is inside — the table or an open cell editor.
    fn on_key(&mut self, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &e.keystroke;
        let m = ks.modifiers;
        if self.editor.is_some() {
            // Focus decides whose grammar a key speaks. The WHERE strip
            // and the popovers live inside this pane too; their
            // keystrokes bubble through here and are not ours.
            let (focused, replace) = self
                .editor
                .as_ref()
                .map(|ed| (ed.input.focus_handle(cx).is_focused(window), ed.replace))
                .unwrap_or((false, false));
            if !focused {
                return;
            }
            match ks.key.as_str() {
                "escape" => {
                    // One Esc right after ⌘N, nothing typed: the new row
                    // goes with the editor. With text typed, this Esc
                    // discards only the text — one Esc never takes two
                    // things — and the next one dismisses the row.
                    let row = self
                        .editor
                        .as_ref()
                        .filter(|ed| ed.input.read(cx).value().is_empty())
                        .map(|ed| ed.row);
                    self.cancel_edit(cx);
                    if row.is_some_and(|row| self.dismiss_untouched_draft(row, cx)) {
                        self.clear_ring(cx);
                    }
                }
                // The newline family (Steve's ruling): ⇧Enter first —
                // the chat-composer convention every hand knows — with
                // ⌥Enter as the Sheets twin. They belong to the text.
                // (Visibly inert until the multi-line editor arrives —
                // reserved is not dead.)
                "enter" if m.shift || m.alt => return,
                "enter" if m.platform => {
                    // ⌘Enter — "send it" (normally consumed by the input
                    // and routed via PressEnter{secondary}; this arm is
                    // the backstop).
                    if self.confirm_and_move(0, 0, cx) {
                        self.commit(cx);
                    }
                }
                "enter" => {
                    self.confirm_and_move(1, 0, cx);
                }
                "tab" => {
                    self.confirm_and_tab(if m.shift { -1 } else { 1 }, window, cx);
                }
                "up" if replace => {
                    self.confirm_and_move(-1, 0, cx);
                }
                "down" if replace => {
                    self.confirm_and_move(1, 0, cx);
                }
                "left" if replace => {
                    self.confirm_and_move(0, -1, cx);
                }
                "right" if replace => {
                    self.confirm_and_move(0, 1, cx);
                }
                "s" if m.platform => {
                    // "I'm done, make it real": confirm in place, commit.
                    if self.confirm_and_move(0, 0, cx) {
                        self.commit(cx);
                    }
                }
                _ => return,
            }
            cx.stop_propagation();
            return;
        }
        // Navigating — but only when the table itself holds focus. A key
        // typed into the WHERE input (or any other input in the pane)
        // must mean what that input says it means.
        if !self.table.focus_handle(cx).contains_focused(window, cx) {
            return;
        }
        // ⌘S and ⌘Enter both mean "send it" — the file-save reflex and
        // the AI-composer reflex arrive at the same transaction.
        if m.platform && !m.shift && (ks.key == "s" || ks.key == "enter") {
            self.commit(cx);
            cx.stop_propagation();
            return;
        }
        if m.platform && ks.key == "z" {
            let did = match &mut self.edits {
                // The statements were built at ⌘S; an undo now would show
                // the edit gone while the commit writes it anyway.
                _ if self.committing => false,
                // A held set has no history to walk: the reason is said.
                Some(e) if e.in_doubt() => {
                    self.error = stale_reason(self.reshaped, false, true).map(str::to_string);
                    cx.notify();
                    false
                }
                Some(e) if m.shift => e.redo(),
                Some(e) => e.undo(),
                None => false,
            };
            if did {
                self.sync_staged(cx);
            }
            cx.stop_propagation();
            return;
        }
        if m.platform && m.shift && ks.key == "backspace" {
            // ⌘⇧⌫, TablePlus's own chord: discard everything staged —
            // one undo entry, so even this is reversible. A held set is not
            // discarded from the keyboard: dropping it is a verdict that its
            // commit landed, given in the review popover.
            self.discard_all(cx);
            cx.stop_propagation();
            return;
        }
        if m.platform && ks.key == "backspace" {
            self.stage_delete_row(cx);
            cx.stop_propagation();
            return;
        }
        if m.control && m.shift && ks.key == "n" {
            self.stage_null(cx);
            cx.stop_propagation();
            return;
        }
        // (⌥←/⌥→, the view-switcher carousel, live at App level in
        // main.rs — they must keep working in Structure mode, where
        // this listener's focus source doesn't exist.)
        // A navigation chord with no ring on the page yet: assume the
        // journey starts at the top-left cell — Sheets' A1 assumption —
        // and let the chord mean what it means from there (⌘↓ reaches
        // the last row in one press, ⌥↓ flips the page). Bare arrows
        // never arrive here — the interceptor in Grid::build owns them,
        // where a first press just takes the seat. Navigation only:
        // keys that edit (Enter, ⌫, typing) still require a
        // deliberately chosen cell, and ⇧ combos stay inert with range
        // selection reserved.
        let nav_key = matches!(
            ks.key.as_str(),
            "up" | "down" | "left" | "right" | "home" | "end" | "pageup" | "pagedown"
        );
        if nav_key && !m.shift && !m.control {
            self.seed_ring(cx);
        }
        // The modified-arrow grammar (docs/EDITING.md "Navigation"), all
        // of it needing a cell to move — which the seed above guarantees
        // a navigation key always has. JUMP rides the ring's own clamp:
        // an impossible distance lands exactly on the edge.
        const JUMP: i32 = 1_000_000;
        if self.table.read(cx).delegate().active_cell.is_some() {
            // ⌘-arrows jump to the edges of the page — Sheets muscle
            // memory, scoped the way fit is: to what you're looking at.
            // (⌘⇧-arrows stay inert with the other ⇧ combos: extending a
            // selection to the edge is range territory, reserved.)
            if m.platform && !m.alt && !m.control && !m.shift {
                let (dr, dc) = match ks.key.as_str() {
                    "up" => (-JUMP, 0),
                    "down" => (JUMP, 0),
                    "left" => (0, -JUMP),
                    "right" => (0, JUMP),
                    _ => (0, 0),
                };
                if (dr, dc) != (0, 0) {
                    self.move_ring(dr, dc, cx);
                    cx.stop_propagation();
                    return;
                }
            }
            // Fn-arrows: Home/End are the column edges; the Page keys
            // drive the pager, the ring keeping its seat across the flip.
            // ⌥↑/⌥↓ alias the Page keys — reachable without Fn.
            match ks.key.as_str() {
                // Home/End take the row's edges; with ⌘ they take the
                // page's corners (Sheets' ⌘Home = A1, ⌘End = end of
                // data, scoped to the page like every other jump).
                "home" => {
                    self.move_ring(if m.platform { -JUMP } else { 0 }, -JUMP, cx);
                    cx.stop_propagation();
                    return;
                }
                "end" => {
                    self.move_ring(if m.platform { JUMP } else { 0 }, JUMP, cx);
                    cx.stop_propagation();
                    return;
                }
                // F2, the third door into the kept-value editor (Sheets
                // and every clone bind it) — and the one that works
                // mid-Tab-run, where Enter means carriage return.
                "f2" => {
                    if let Some((row, col)) = self.table.read(cx).delegate().active_cell {
                        self.open_editor(row, col, None, window, cx);
                    }
                    cx.stop_propagation();
                    return;
                }
                "pageup" => {
                    self.page_screen(-1, cx);
                    cx.stop_propagation();
                    return;
                }
                "pagedown" => {
                    self.page_screen(1, cx);
                    cx.stop_propagation();
                    return;
                }
                "up" if m.alt && !m.platform && !m.control => {
                    self.page_step(-1, cx);
                    cx.stop_propagation();
                    return;
                }
                "down" if m.alt && !m.platform && !m.control => {
                    self.page_step(1, cx);
                    cx.stop_propagation();
                    return;
                }
                _ => {}
            }
        }
        if m.platform || m.control || m.function {
            return; // chords we don't own keep their meanings
        }
        let Some((row, col)) = self.table.read(cx).delegate().active_cell else {
            return;
        };
        // ⇧-arrows are deliberately inert: they are range selection's
        // seat (deferred), and a ring that moves when you expected a
        // range to grow would lie. A dead key teaches honestly.
        match ks.key.as_str() {
            "enter" => {
                // Sheets' split personality, faithfully: Enter normally
                // opens the editor keeping the value — but during a Tab
                // run it is a carriage return, sweeping to the run's
                // anchor column one row on, no editor.
                let anchor = self.table.read(cx).delegate().tab_anchor;
                match anchor {
                    Some(a) => self.sweep(if m.shift { -1 } else { 1 }, a, cx),
                    None => self.open_editor(row, col, None, window, cx),
                }
                cx.stop_propagation();
            }
            "backspace" | "delete" => {
                self.stage_clear(row, col, cx);
                cx.stop_propagation();
            }
            "up" if !m.shift && !m.alt => {
                self.move_ring(-1, 0, cx);
                cx.stop_propagation();
            }
            "down" if !m.shift && !m.alt => {
                self.move_ring(1, 0, cx);
                cx.stop_propagation();
            }
            "left" if !m.shift && !m.alt => {
                self.move_ring(0, -1, cx);
                cx.stop_propagation();
            }
            "right" if !m.shift && !m.alt => {
                self.move_ring(0, 1, cx);
                cx.stop_propagation();
            }
            "tab" => {
                self.tab_move(if m.shift { -1 } else { 1 }, cx);
                cx.stop_propagation();
            }
            "escape" => {
                self.escape_ring(cx);
                cx.stop_propagation();
            }
            _ => {
                // The typing contract: a printable character opens the
                // editor seeded with itself — replace entry.
                if let Some(ch) = &ks.key_char {
                    if !ch.chars().all(char::is_control) && self.edits.is_some() {
                        self.open_editor(row, col, Some(ch.clone()), window, cx);
                        cx.stop_propagation();
                    }
                }
            }
        }
    }

    /// A focused Input maps Tab / Shift-Tab to these actions before the
    /// raw KeyDownEvent can bubble to `on_key`. Catch them on the cell
    /// editor's ancestor and run the same confirm-and-wrap path as the
    /// grid-level keyboard handler.
    fn on_editor_tab(
        &mut self,
        _: &IndentInline,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editor.as_ref().is_some_and(|ed| ed.input.focus_handle(cx).is_focused(window)) {
            self.confirm_and_tab(1, window, cx);
        } else {
            cx.propagate();
        }
    }

    fn on_editor_shift_tab(
        &mut self,
        _: &OutdentInline,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editor.as_ref().is_some_and(|ed| ed.input.focus_handle(cx).is_focused(window)) {
            self.confirm_and_tab(-1, window, cx);
        } else {
            cx.propagate();
        }
    }

    /// A double-click in the table body opens the kept-value editor on
    /// the cell the first click just made active (the delegate's own
    /// mouse-down runs before this bubbling listener).
    fn on_body_click(&mut self, e: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Click-away confirms — the universal grid contract (Sheets,
        // Excel, AG Grid: focus moving to another cell commits the
        // edit). The clicked cell's own mouse-down has already moved
        // active_cell by the time this bubbles, so "editing != active"
        // is precisely "the click landed elsewhere"; a click inside the
        // open editor moves nothing and stays an editor click. Confirm
        // stages the value and hands focus back to the table, so the
        // next keystroke types into the newly ringed cell. (A value
        // that fails validation keeps its editor and its reason — Esc
        // remains the way out of a bad value.)
        if self.editor.is_some() {
            let (editing, active) = {
                let d = self.table.read(cx).delegate();
                (d.editing, d.active_cell)
            };
            if editing != active {
                self.confirm_and_move(0, 0, cx);
            }
        }
        if e.click_count != 2 || self.editor.is_some() {
            return;
        }
        // The header row is the fit gesture's turf (divider_double_click)
        // — its recorded frame excludes body double-clicks from opening
        // an editor and vice versa.
        if self.table_bounds.get().contains(&e.position) {
            return;
        }
        if let Some((row, col)) = self.table.read(cx).delegate().active_cell {
            self.open_editor(row, col, None, window, cx);
        }
    }

    /// Open the cell editor. `seed` = replace entry (the typed
    /// character); None = kept-value entry (Enter / double-click).
    fn open_editor(
        &mut self,
        row: usize,
        col: usize,
        seed: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.edits.is_none() || self.committing {
            return; // read-only says why in the footer, not with a beep
        }
        if self.refuse_stale(cx) {
            return;
        }
        if self.generated.get(col).copied().unwrap_or(false) {
            let name = self.table.read(cx).delegate().names[col].clone();
            self.error = Some(format!("{name} is generated by DuckDB"));
            cx.notify();
            return;
        }
        let (original, deleted, draft_key) = {
            let d = self.table.read(cx).delegate();
            if row >= d.rows.len() {
                return;
            }
            let base = d.rows[row].get(col).cloned().flatten();
            let draft_key = d.draft_key(row).map(str::to_string);
            let staged = if draft_key.is_some() {
                d.draft_cells.get(&(row, col)).cloned()
            } else {
                d.staged.get(&(row, col)).cloned()
            };
            (
                staged.unwrap_or(base),
                d.deleted.contains(&row),
                draft_key,
            )
        };
        if deleted {
            return; // you cannot edit a ghost; revert the delete first (⌘Z)
        }
        // Closing the preceding editor schedules table focus for the next
        // frame. A Tab run opens its neighbor immediately, so cancel that
        // handoff before it can steal focus back from the new input.
        self.needs_focus = false;
        let replace = seed.is_some();
        let text = seed
            .unwrap_or_else(|| original.as_ref().map(|s| s.to_string()).unwrap_or_default());
        let input = cx.new(|cx| {
            gpui_kit::component::input::InputState::new(window, cx).default_value(text)
        });
        input.update(cx, |state, cx| {
            // Caret at the end (set_cursor_position also focuses):
            // replace entry keeps typing past its seed; kept-value entry
            // lands where Sheets puts it. The column clamps to the line.
            state.set_cursor_position(
                gpui_kit::component::input::Position::new(0, u32::MAX),
                window,
                cx,
            );
        });
        // Enter may be consumed by the input before it bubbles; the event
        // subscription is the belt to on_key's suspenders. Idempotent:
        // whoever runs first takes the editor.
        cx.subscribe(&input, |grid, _, ev: &gpui_kit::component::input::InputEvent, cx| {
            if let gpui_kit::component::input::InputEvent::PressEnter { secondary, .. } = ev {
                if *secondary {
                    // ⌘Enter — "send it", the AI-era universal: confirm
                    // this cell, then commit everything staged.
                    if grid.confirm_and_move(0, 0, cx) {
                        grid.commit(cx);
                    }
                } else {
                    grid.confirm_and_move(1, 0, cx);
                }
            }
        })
        .detach();
        self.editor = Some(CellEditor {
            row,
            col,
            draft_key,
            input: input.clone(),
            replace,
        });
        let grid = cx.entity().downgrade();
        self.table.update(cx, |state, cx| {
            let d = state.delegate_mut();
            d.editing = Some((row, col));
            d.editor_input = Some(input.clone());
            d.pick = Some(Rc::new({
                let grid = grid.clone();
                move |value: SharedString, window: &mut Window, cx: &mut App| {
                    input.update(cx, |state, cx| state.set_value(value, window, cx));
                    if let Some(grid) = grid.upgrade() {
                        grid.update(cx, |grid, cx| grid.confirm_and_move(0, 0, cx));
                    }
                }
            }));
            d.rebuild_cols();
            state.refresh(cx);
            cx.notify();
        });
        cx.notify();
    }

    /// Tab while editing is a continuous entry gesture: confirm the current
    /// cell, wrap the ring to its neighbor, then immediately open that cell's
    /// editor. Validation failure leaves the original editor in place.
    fn confirm_and_tab(&mut self, dc: i32, window: &mut Window, cx: &mut Context<Self>) {
        // Resolve the destination BEFORE confirmation projects the new staged
        // value into the table. A dirty row intentionally has no library row
        // selection, so the cell coordinates—not selection paint—are the
        // durable source of truth for this horizontal move.
        let Some((row, next_col, next_pos, anchor)) = self.editor.as_ref().and_then(|ed| {
            let d = self.table.read(cx).delegate();
            let pos = d.visible.iter().position(|&col| col == ed.col)?;
            let next_pos = wrapped_step(d.visible.len(), pos, dc);
            Some((
                ed.row,
                d.visible[next_pos],
                next_pos,
                d.tab_anchor.or((dc > 0).then_some(ed.col)),
            ))
        }) else {
            return;
        };
        if !self.confirm_and_move(0, 0, cx) {
            return;
        }
        self.table.update(cx, |state, cx| {
            let d = state.delegate_mut();
            d.tab_anchor = anchor;
            d.active_cell = Some((row, next_col));
            let gutter = d.gutter as usize;
            select_row(state, row, cx);
            state.scroll_to_col(next_pos + gutter, cx);
            cx.notify();
        });
        self.header_chase = true;
        self.open_editor(row, next_col, None, window, cx);
    }

    /// Esc: the in-progress text never happened; what was there before —
    /// staged value or fetched value — is still there. Ring stays put.
    fn cancel_edit(&mut self, cx: &mut Context<Self>) {
        self.editor = None;
        self.close_editor_cell(cx);
    }

    fn close_editor_cell(&mut self, cx: &mut Context<Self>) {
        self.table.update(cx, |state, cx| {
            let d = state.delegate_mut();
            d.editing = None;
            d.editor_input = None;
            d.pick = None;
            d.rebuild_cols();
            state.refresh(cx);
            cx.notify();
        });
        self.needs_focus = true;
        cx.notify();
    }

    /// Confirm the open editor: decide what the text does to the cell
    /// (`edits::confirm`), stage it, close, move the ring. A vertical confirm
    /// during a Tab run sweeps back to the anchor column (Sheets' carriage
    /// return). Returns false when validation refused — the editor stays
    /// open with the reason.
    fn confirm_and_move(
        &mut self,
        dr: i32,
        dc: i32,
        cx: &mut Context<Self>,
    ) -> bool {
        // Staging now would land in a set the commit clears when it succeeds.
        // The editor stays open, and Enter works once the commit settles.
        if self.committing {
            return false;
        }
        let Some(ed) = self.editor.take() else { return true };
        let text = ed.input.read(cx).value().to_string();
        let (ty, fetched, staged, identity) = {
            let d = self.table.read(cx).delegate();
            let at = (ed.row, ed.col);
            let ty = d.schema_cols.get(ed.col).map(|c| c.duckdb_type.clone()).unwrap_or_default();
            // A draft's row is its own cells: nothing in it was fetched.
            if ed.draft_key.is_some() {
                (ty, None, d.draft_cells.get(&at).cloned(), None)
            } else {
                (
                    ty,
                    d.rows.get(ed.row).and_then(|r| r.get(ed.col)).cloned().flatten(),
                    d.staged.get(&at).cloned(),
                    d.identities.get(ed.row).cloned(),
                )
            }
        };
        let copied = match (&self.edits, &ed.draft_key) {
            (Some(edits), Some(key)) => edits.copied_text(key, ed.col),
            _ => None,
        };
        fn shown(text: &Option<SharedString>) -> Option<&str> {
            text.as_ref().map(|t| t.as_ref())
        }
        let verdict = edits::confirm(
            &text,
            &edits::Held {
                ty: &ty,
                draft: ed.draft_key.is_some(),
                fetched: shown(&fetched),
                staged: staged.as_ref().map(shown),
                copied: copied.as_ref().map(shown),
            },
        );
        match verdict {
            edits::Confirm::Keep => self.error = None,
            edits::Confirm::Refuse(reason) => {
                // Validation informs, never imprisons: the editor stays
                // open with the reason; Esc still works.
                self.error = Some(reason);
                self.editor = Some(ed);
                cx.notify();
                return false;
            }
            edits::Confirm::Stage(None, _) if !self.stageable_null(ed.col, cx) => {
                self.editor = Some(ed);
                return false;
            }
            edits::Confirm::Revert | edits::Confirm::Stage(..) => {
                if let Some(edits) = &mut self.edits {
                    self.error = None;
                    match (verdict, &ed.draft_key, identity) {
                        (edits::Confirm::Stage(text, value), Some(key), _) => {
                            edits.stage_insert_cell(key, ed.col, text, value)
                        }
                        (edits::Confirm::Stage(text, value), None, Some(identity)) => {
                            edits.stage_cell(identity, ed.col, fetched, text, value)
                        }
                        (edits::Confirm::Revert, Some(key), _) => edits.stage_insert_copied(key, ed.col),
                        // Staging what was fetched is what drops a staged edit.
                        (edits::Confirm::Revert, None, Some(identity)) => {
                            edits.stage_cell(identity, ed.col, fetched.clone(), fetched, Value::Null)
                        }
                        _ => {}
                    }
                }
            }
        }
        self.close_editor_cell(cx);
        self.sync_staged(cx);
        if dr != 0 && dc == 0 {
            let anchor = self.table.read(cx).delegate().tab_anchor;
            match anchor {
                Some(col) => self.sweep(dr, col, cx),
                None => self.move_ring(dr, 0, cx),
            }
        } else if dr != 0 || dc != 0 {
            self.move_ring(dr, dc, cx);
        }
        true
    }

    /// NOT NULL columns refuse a staged NULL at the fingers, with the
    /// reason where the eyes are.
    fn stageable_null(&mut self, col: usize, cx: &mut Context<Self>) -> bool {
        if self.not_null.get(col).copied().unwrap_or(false) {
            let name = self.table.read(cx).delegate().names[col].clone();
            self.error = Some(format!("{name} is NOT NULL — edit the value instead"));
            cx.notify();
            return false;
        }
        true
    }

    /// Delete on a cell: clear it, type-honestly — '' for text columns,
    /// NULL for everything else. Never touches the row.
    fn stage_clear(&mut self, row: usize, col: usize, cx: &mut Context<Self>) {
        if self.committing || self.edits.is_some() && self.refuse_stale(cx) {
            return;
        }
        if self.generated.get(col).copied().unwrap_or(false) {
            let name = self.table.read(cx).delegate().names[col].clone();
            self.error = Some(format!("{name} is generated by DuckDB"));
            cx.notify();
            return;
        }
        let (ty, fetched, identity, draft_key, deleted) = {
            let d = self.table.read(cx).delegate();
            (
                d.schema_cols.get(col).map(|c| c.duckdb_type.clone()).unwrap_or_default(),
                d.rows.get(row).and_then(|r| r.get(col)).cloned().flatten(),
                d.identities.get(row).cloned(),
                d.draft_key(row).map(str::to_string),
                d.deleted.contains(&row),
            )
        };
        if deleted {
            return;
        }
        let (text, value) = if edits::is_text_type(&ty) {
            (Some(SharedString::from("")), Value::String(String::new()))
        } else {
            if !self.stageable_null(col, cx) {
                return;
            }
            (None, Value::Null)
        };
        if let Some(edits) = &mut self.edits {
            self.error = None;
            if let Some(key) = draft_key {
                edits.stage_insert_cell(&key, col, text, value);
            } else if let Some(identity) = identity {
                edits.stage_cell(identity, col, fetched, text, value);
            }
            self.sync_staged(cx);
        }
    }

    /// ⌃⇧N: SQL NULL, deliberately, any column type.
    fn stage_null(&mut self, cx: &mut Context<Self>) {
        let Some((row, col)) = self.table.read(cx).delegate().active_cell else { return };
        if self.committing || self.edits.is_some() && self.refuse_stale(cx) {
            return;
        }
        if self.generated.get(col).copied().unwrap_or(false) {
            let name = self.table.read(cx).delegate().names[col].clone();
            self.error = Some(format!("{name} is generated by DuckDB"));
            cx.notify();
            return;
        }
        let (fetched, identity, draft_key, deleted) = {
            let d = self.table.read(cx).delegate();
            (
                d.rows.get(row).and_then(|r| r.get(col)).cloned().flatten(),
                d.identities.get(row).cloned(),
                d.draft_key(row).map(str::to_string),
                d.deleted.contains(&row),
            )
        };
        if deleted {
            return; // the row is a ghost, as for Delete and the editor
        }
        if !self.stageable_null(col, cx) {
            return;
        }
        if let Some(edits) = &mut self.edits {
            if let Some(key) = draft_key {
                edits.stage_insert_cell(&key, col, None, Value::Null);
            } else if let Some(identity) = identity {
                edits.stage_cell(identity, col, fetched, None, Value::Null);
            }
            self.sync_staged(cx);
        }
    }

    /// Edit-menu entry point; the window argument keeps its signature
    /// parallel with New Row for the shared App-level dispatcher.
    pub(crate) fn delete_row(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.stage_delete_row(cx);
    }

    /// Stage a DELETE for every selected row — visible, ghosted,
    /// reversible until commit. No dialog: reversibility is the
    /// confirmation model. Each row is its own staged change for the
    /// review popover; the gesture is one ⌘Z.
    fn stage_delete_row(&mut self, cx: &mut Context<Self>) {
        if self.committing {
            return;
        }
        let targets: Vec<(Option<Vec<Value>>, Option<String>)> = {
            let d = self.table.read(cx).delegate();
            let rows: Vec<usize> = if d.selection.rows.is_empty() {
                d.active_cell.map(|(r, _)| r).into_iter().collect()
            } else {
                d.selection.rows.iter().copied().collect()
            };
            rows.into_iter()
                .map(|row| (d.identities.get(row).cloned(), d.draft_key(row).map(str::to_string)))
                .collect()
        };
        // Removing a draft is a discard, which a reshaped table allows;
        // staging a DELETE is not.
        let reshaped = self.reshaped || self.unrefreshed || self.in_doubt();
        let Some(edits) = &mut self.edits else { return };
        edits.grouped(|edits| {
            for (identity, draft_key) in targets {
                if let Some(key) = draft_key {
                    edits.discard(&key);
                } else if let (Some(identity), false) = (identity, reshaped) {
                    edits.stage_delete(identity);
                }
            }
        });
        self.refuse_stale(cx);
        self.sync_staged(cx);
    }

    /// PageUp/PageDown: one screenful within the loaded page — Sheets'
    /// own meaning for these keys — with a row of overlap for context.
    /// The ring rides; move_ring's clamp stops it at the page's edge
    /// (crossing database pages is ⌥↑/⌥↓'s job, deliberately distinct).
    fn page_screen(&mut self, dir: i32, cx: &mut Context<Self>) {
        let viewport = self
            .table
            .read(cx)
            .vertical_scroll_handle
            .0
            .borrow()
            .base_handle
            .bounds()
            .size
            .height;
        let row_h = prefs::get(cx).table_size().table_row_height();
        let rows = ((viewport / row_h).floor() as i32 - 1).max(1);
        self.move_ring(dir * rows, 0, cx);
    }

    /// ⌥↑/⌥↓ from the keyboard: flip the database page and let the ring
    /// keep its seat — same column, same row position (clamped), new
    /// rows. A flip that cannot happen is a quiet no-op.
    fn page_step(&mut self, delta: i32, cx: &mut Context<Self>) {
        let can = if delta < 0 { self.page > 0 } else { self.has_next(cx) };
        if !can {
            return;
        }
        self.ring_keep = self.table.read(cx).delegate().active_cell;
        let page = if delta < 0 { self.page - 1 } else { self.page + 1 };
        self.fetch_page(page, cx);
    }

    /// Ask for table focus on the next frame (render consumes the flag,
    /// where a &mut Window exists) — the view switcher calls this when
    /// landing back on Data.
    pub(crate) fn request_focus(&mut self, cx: &mut Context<Self>) {
        self.needs_focus = true;
        cx.notify();
    }

    /// Esc while navigating: clear the ring, the selection, and any Tab
    /// run — the same panic key, the same "nothing happened" result.
    /// Esc while navigating: a draft row nothing has been entered into,
    /// under the ring, is dismissed; then the ring and selection clear.
    fn escape_ring(&mut self, cx: &mut Context<Self>) {
        let row = {
            let d = self.table.read(cx).delegate();
            d.active_cell.map(|(row, _)| row).or(d.selection.lead)
        };
        if let Some(row) = row {
            self.dismiss_untouched_draft(row, cx);
        }
        self.clear_ring(cx);
    }

    /// Remove `row` if it is a draft nothing has been entered into. It
    /// holds nothing typed, so Esc loses nothing by it, and ⌘Z brings it
    /// back. A draft with any entered value — a duplicate's copied cells
    /// included — stays; ⌘⌫ discards it. Returns whether it went.
    fn dismiss_untouched_draft(&mut self, row: usize, cx: &mut Context<Self>) -> bool {
        if self.committing {
            return false;
        }
        let Some(key) = self.table.read(cx).delegate().draft_key(row).map(str::to_string) else {
            return false;
        };
        let Some(edits) = &mut self.edits else { return false };
        if !edits.is_untouched_insert(&key) {
            return false;
        }
        edits.grouped(|edits| edits.discard(&key));
        self.sync_staged(cx);
        true
    }

    fn clear_ring(&mut self, cx: &mut Context<Self>) {
        self.table.update(cx, |state, cx| {
            let d = state.delegate_mut();
            d.active_cell = None;
            d.tab_anchor = None;
            // Cleared here, not left to the reconcile observer: a dirty
            // lead deliberately has no library selection to lose, so the
            // observer would read the Escape as nothing having happened.
            d.selection.clear();
            state.clear_selection(cx);
            cx.notify();
        });
        cx.notify();
    }

    /// Tab / ⇧Tab: move along the row with wraparound, remembering where
    /// the run began (Sheets' typewriter anchor) so Enter can sweep back
    /// to it. Tab at the right edge returns to the first visible cell;
    /// Shift-Tab at the left edge returns to the last.
    fn tab_move(&mut self, dc: i32, cx: &mut Context<Self>) {
        self.table.update(cx, |state, cx| {
            let d = state.delegate_mut();
            let Some((row, col)) = d.active_cell else { return };
            if d.visible.is_empty() {
                return;
            }
            let pos = d.visible.iter().position(|&visible| visible == col).unwrap_or(0);
            let next = wrapped_step(d.visible.len(), pos, dc);
            let next_col = d.visible[next];
            // A Tab keeps the original anchor if the run was already
            // going. Only a FORWARD Tab starts a run (the Excel/Univer
            // reference rule): Shift-Tab retreats within one but never
            // begins one.
            d.tab_anchor = d.tab_anchor.or((dc > 0).then_some(col));
            d.active_cell = Some((row, next_col));
            let display_col = next + d.gutter as usize;
            select_row(state, row, cx);
            state.scroll_to_col(display_col, cx);
            cx.notify();
        });
        self.header_chase = true;
        cx.notify();
    }

    /// The carriage return: Enter after a Tab run goes back to the run's
    /// anchor column, one row on — the typewriter physics that makes
    /// entering a row of data feel effortless in Sheets.
    fn sweep(&mut self, dr: i32, col: usize, cx: &mut Context<Self>) {
        self.table.update(cx, |state, cx| {
            let d = state.delegate_mut();
            d.tab_anchor = None;
            let Some((r, _)) = d.active_cell else { return };
            if d.rows.is_empty() || d.visible.is_empty() {
                return;
            }
            let nr = (r as i32 + dr).clamp(0, d.rows.len() as i32 - 1) as usize;
            let nc = if d.visible.contains(&col) { col } else { d.visible[0] };
            let np = d.visible.iter().position(|&v| v == nc).unwrap_or(0);
            let gutter = d.gutter as usize;
            d.active_cell = Some((nr, nc));
            select_row(state, nr, cx);
            // The carriage return crosses most of the row — bring the
            // anchor column back into view with it.
            state.scroll_to_col(np + gutter, cx);
            cx.notify();
        });
        self.header_chase = true;
        cx.notify();
    }

    /// The A1 assumption (Steve's ruling): a navigation gesture with no
    /// ring on the page yet starts at the top-left cell. Returns whether
    /// it placed the ring — a bare arrow's first press consumes itself
    /// on that placement (the interceptor checks), while chords apply
    /// their motion from the fresh seat (⌘↓ still reaches the last row
    /// in one press). An empty page seats nobody.
    fn seed_ring(&mut self, cx: &mut Context<Self>) -> bool {
        let mut seeded = false;
        self.table.update(cx, |state, cx| {
            let d = state.delegate_mut();
            if d.active_cell.is_none() && !d.rows.is_empty() && !d.visible.is_empty() {
                d.active_cell = Some((0, d.visible[0]));
                seeded = true;
                select_row(state, 0, cx);
                cx.notify();
            }
        });
        seeded
    }

    /// Move the active-cell ring. Columns move along the VISIBLE order,
    /// so hidden columns don't swallow a keystroke. Any ring move ends a
    /// Tab run (tab_move re-arms after calling this).
    fn move_ring(&mut self, dr: i32, dc: i32, cx: &mut Context<Self>) {
        self.table.update(cx, |state, cx| {
            let d = state.delegate_mut();
            d.tab_anchor = None;
            let Some((r, c)) = d.active_cell else { return };
            if d.rows.is_empty() || d.visible.is_empty() {
                return;
            }
            let nr = (r as i32 + dr).clamp(0, d.rows.len() as i32 - 1) as usize;
            let pos = d.visible.iter().position(|&v| v == c).unwrap_or(0);
            let np = (pos as i32 + dc).clamp(0, d.visible.len() as i32 - 1) as usize;
            let nc = d.visible[np];
            let gutter = d.gutter as usize;
            d.active_cell = Some((nr, nc));
            select_row(state, nr, cx);
            if dc != 0 {
                // The viewport follows the ring sideways too — minimal
                // scroll, so a jump to the far edge lands the cell at
                // the visible edge and a one-step move only scrolls
                // when crossing it (select_row covers the vertical).
                state.scroll_to_col(np + gutter, cx);
            }
            cx.notify();
        });
        if dc != 0 {
            self.header_chase = true;
        }
        cx.notify();
    }

    /// Show the staging model after a gesture changed it. When that gesture
    /// removed the last edit staged against columns the table does not have
    /// (`reshaped`), the table is fetched as it is, and this time adopted.
    /// The flag outlives a fetch that fails, so nothing is staged or
    /// committed against the old columns while the retry is pending; only
    /// the fetch that succeeds lowers it.
    fn sync_staged(&mut self, cx: &mut Context<Self>) {
        self.project_staged(cx);
        if self.reshaped && !self.edits.as_ref().is_some_and(Edits::any_staged) {
            self.fetch_page_now(self.page, cx);
        }
    }

    /// Project the staging model into the grid. INSERT drafts are
    /// synthetic rows before the fetched page; existing-row changes land
    /// by identity wherever (and whether) their rows currently appear.
    ///
    /// A set held because its commit got no answer is not drawn at all. It
    /// may have landed, and its keys were read before it did: a staged
    /// DELETE of id 7 would ghost the row a staged re-key of 3 to 7 made,
    /// and that re-key would show on no row. The page shows the database,
    /// and the set is reviewed in the popover (`judge_held`).
    ///
    /// While the page predates a commit that landed (`unrefreshed`), the
    /// rows it deleted keep their ghosts: nothing is staged to redraw them
    /// from, and without them they would look alive. Nothing is staged in
    /// that state, since the commit cleared the set and the page takes no
    /// staging; with a set staged, the ghosts are the set's own.
    fn project_staged(&mut self, cx: &mut Context<Self>) {
        let keep_ghosts = self.unrefreshed && !self.edits.as_ref().is_some_and(Edits::any_staged);
        let changes: Vec<(String, Vec<Value>, edits::RowChange)> = self
            .edits
            .as_ref()
            .map(|e| {
                e.projection()
                    .into_iter()
                    .map(|(key, identity, change)| {
                        (key.to_string(), identity.to_vec(), change.clone())
                    })
                    .collect()
            })
            .unwrap_or_default();
        let next_drafts: Vec<String> = changes
            .iter()
            .filter(|(_, _, change)| matches!(change, edits::RowChange::Insert(_)))
            .map(|(key, _, _)| key.clone())
            .collect();
        let draft_shape_changed =
            self.table.read(cx).delegate().draft_keys.as_slice() != next_drafts.as_slice();
        if draft_shape_changed {
            self.editor = None;
            self.needs_focus = true;
        }
        self.table.update(cx, |state, cx| {
            let d = state.delegate_mut();
            d.remove_drafts();
            d.staged.clear();
            if !keep_ghosts {
                d.deleted.clear();
            }

            let width = d.schema_cols.len();
            let mut draft_rows = Vec::new();
            for (key, _, change) in &changes {
                let edits::RowChange::Insert(cells) = change else { continue };
                let row = draft_rows.len();
                let mut values = vec![None; width];
                for (col, cell) in cells {
                    if *col < width {
                        values[*col] = cell.text.clone();
                        d.draft_cells.insert((row, *col), cell.text.clone());
                    }
                }
                d.draft_keys.push(key.clone());
                draft_rows.push(values);
            }
            let draft_count = draft_rows.len();
            draft_rows.append(&mut d.rows);
            d.rows = draft_rows;
            let mut identities = vec![Vec::new(); draft_count];
            identities.append(&mut d.identities);
            d.identities = identities;
            let mut labels = vec![SharedString::from("+"); draft_count];
            labels.append(&mut d.row_labels);
            d.row_labels = labels;

            // Fetched identities shifted down by the inserted drafts.
            d.row_of = d
                .identities
                .iter()
                .enumerate()
                .skip(draft_count)
                .map(|(ix, id)| (edits::key_of(id), ix))
                .collect();
            for (key, _, change) in &changes {
                let Some(&row) = d.row_of.get(key) else { continue };
                match change {
                    edits::RowChange::Update(cells) => {
                        for (col, cell) in cells {
                            d.staged.insert((row, *col), cell.text.clone());
                        }
                    }
                    edits::RowChange::Delete => {
                        d.deleted.insert(row);
                    }
                    edits::RowChange::Insert(_) => {}
                }
            }
            if draft_shape_changed {
                d.selection.clear();
                d.active_cell = None;
                d.editing = None;
                d.editor_input = None;
                d.pick = None;
                d.tab_anchor = None;
            }
            // Draft hints need room only while a draft or editor is on
            // screen. Rebuild from the compact fitted widths whenever
            // staging changes so the grid grows and shrinks with mode.
            d.rebuild_cols();
            if draft_shape_changed {
                state.clear_selection(cx);
            }
            // Dirtiness just changed under the selection: re-decide the
            // wash (undoing a row's last edit gives its wash back).
            if let Some(row) = state.delegate().selection.lead {
                select_row(state, row, cx);
            }
            state.refresh(cx);
            cx.notify();
        });
        cx.notify();
    }

    /// Whether a cell editor is open: text typed into it is staged only when
    /// it is confirmed, so a quit would lose it.
    pub(crate) fn is_editing(&self) -> bool {
        self.editor.is_some()
    }

    /// Whether the table has other columns than the grid's (`reshaped`).
    pub(crate) fn is_reshaped(&self) -> bool {
        self.reshaped
    }

    /// Whether the staged set is held because its commit got no answer.
    pub(crate) fn in_doubt(&self) -> bool {
        self.edits.as_ref().is_some_and(Edits::in_doubt)
    }

    /// The review popover's two ways out of a held set, each for the whole
    /// set: the commit `landed`, and the set is dropped for good, or it did
    /// not, and the set is staged again, drawn on the page and sent by ⌘S.
    /// Refused with the reason while there is nothing to judge it against
    /// (`verdict_refusal`); the status line is cleared only by a verdict
    /// that was taken.
    pub(crate) fn judge_held(&mut self, landed: bool, cx: &mut Context<Self>) {
        if self.committing {
            return;
        }
        let Some(edits) = self.edits.as_mut().filter(|e| e.in_doubt()) else { return };
        if let Some(reason) = verdict_refusal(landed, self.reshaped, edits.unjudged()) {
            self.error = Some(reason.to_string());
            cx.notify();
            return;
        }
        if edits.judge(landed) {
            self.error = None;
            self.sync_staged(cx);
        }
    }

    /// The user chose to quit during a commit: what gives the commit's
    /// session back, so the server cancels what is running there and rolls
    /// back whatever its COMMIT has not already made permanent. Blocks on
    /// one request; the caller runs it off the main thread.
    pub(crate) fn release_for_quit(&self) -> Option<Box<dyn FnOnce() + Send>> {
        let session = self.commit_session.lock().unwrap_or_else(|p| p.into_inner()).take()?;
        let conn = self.conn.clone();
        Some(Box::new(move || harbor_client::session_release(&conn, &session)))
    }

    /// The staged sets this grid holds: its own, and one parked with it
    /// while its table's first page has not arrived. What a quit would
    /// discard (docs/EDITING.md, "Dialogs").
    pub(crate) fn staged_sets(&self) -> impl Iterator<Item = &Edits> {
        [self.edits.as_ref(), self.parked.as_ref()].into_iter().flatten()
    }

    /// Surrender the staged layer when this grid is being replaced —
    /// only if there is actually something staged to carry. A stash still
    /// parked here was never adopted, and goes back as it came.
    pub(crate) fn take_edits(&mut self) -> Option<Edits> {
        if let Some(parked) = self.parked.take() {
            return Some(parked);
        }
        let e = self.edits.take()?;
        let (inserts, updates, deletes) = e.counts();
        if inserts + updates + deletes == 0 {
            self.edits = Some(e);
            return None;
        }
        Some(e)
    }

    /// Receive a stashed staging set from a previous visit to this
    /// table (`edits::handoff`). Adopted only when the table still has the
    /// same identity and columns — a changed schema orphans the stash rather
    /// than mis-keying it, and says so. A grid whose first fetch failed has
    /// no columns to compare: it keeps the stash until a fetch brings them
    /// (`settle_schema`), or until it is replaced (`take_edits`).
    pub(crate) fn adopt_edits(&mut self, stash: Edits, cx: &mut Context<Self>) {
        let has_columns = !self.table.read(cx).delegate().schema_cols.is_empty();
        match edits::handoff(self.edits.as_ref(), has_columns, &stash) {
            edits::Handoff::Adopt => {
                let mut stash = stash;
                // A held set comes back to a grid whose page was read just
                // before, outside `fetch`: with its commit over, that page is
                // one to judge it by. With the commit perhaps still running,
                // the page is read again, which asks after its session.
                let running = stash.unsettled().is_some();
                if !running {
                    stash.fetched(None);
                }
                self.edits = Some(stash);
                self.sync_staged(cx);
                if running {
                    self.fetch_page_now(self.page, cx);
                }
            }
            edits::Handoff::Hold => self.parked = Some(stash),
            edits::Handoff::Orphan => {
                let (inserts, updates, deletes) = stash.counts();
                self.error = Some(orphaned(inserts + updates + deletes));
                cx.notify();
            }
        }
    }

    /// A held set gives up nothing by a discard: its commit was all or
    /// nothing, and the way out is a verdict on the whole set, in the review
    /// popover (`judge_held`). Says so, and returns true, when the set is held.
    fn refuse_held(&mut self, cx: &mut Context<Self>) -> bool {
        let held = self.in_doubt();
        if held {
            self.error = stale_reason(self.reshaped, false, true).map(str::to_string);
            cx.notify();
        }
        held
    }

    /// Discard one staged row change (the review popover's per-entry ✕).
    /// Itself undoable — nothing is more than one ⌘Z from recovery.
    pub(crate) fn discard_change(&mut self, key: &str, cx: &mut Context<Self>) {
        if self.committing || self.refuse_held(cx) {
            return;
        }
        if let Some(e) = &mut self.edits {
            e.discard(key);
        }
        self.sync_staged(cx);
    }

    /// Discard everything staged — one gesture, one undo step, so ⌘Z
    /// brings all of it back at once.
    pub(crate) fn discard_all(&mut self, cx: &mut Context<Self>) {
        if self.committing || self.refuse_held(cx) {
            return;
        }
        if let Some(e) = &mut self.edits {
            let keys: Vec<String> =
                e.entries().iter().map(|(k, _, _)| k.to_string()).collect();
            e.grouped(|e| {
                for key in keys {
                    e.discard(&key);
                }
            });
        }
        self.sync_staged(cx);
    }

    /// ⌘S: everything staged, one transaction, all or nothing. A Harbor
    /// session pins the connection so BEGIN..COMMIT outlives one request;
    /// every statement must affect exactly one row or the whole thing
    /// rolls back — and the release itself rolls back on any failure.
    pub(crate) fn commit(&mut self, cx: &mut Context<Self>) {
        if self.committing {
            return;
        }
        // Text in an open editor is part of what the user is sending; the
        // review popover's button reaches here without confirming it.
        if !self.settle_editor(cx) {
            return;
        }
        if self.edits.is_some() && self.refuse_stale(cx) {
            return;
        }
        // A set whose COMMIT got no answer may already be in the database,
        // and sending it again would insert every new row twice. It is held
        // until it has been reviewed and staged again; `refuse_stale` above
        // has said so.
        let Some(edits) = &self.edits else { return };
        let missing = edits.first_missing_required(
            &self.not_null,
            &self.defaults,
            &self.generated,
        );
        if let Some((key, col)) = missing {
            let name = self
                .table
                .read(cx)
                .delegate()
                .names
                .get(col)
                .cloned()
                .unwrap_or_else(|| SharedString::from("column"));
            self.error = Some(format!("{name} is required for the new row · edits kept"));
            self.remap_columns(cx, |d| d.hidden.remove(&col));
            self.table.update(cx, |state, cx| {
                let d = state.delegate_mut();
                if let Some(row) = d.draft_keys.iter().position(|k| k == &key) {
                    d.active_cell = Some((row, col));
                    select_row(state, row, cx);
                }
                cx.notify();
            });
            cx.notify();
            return;
        }
        let stmts = edits.statements();
        if stmts.is_empty() {
            return;
        }
        self.committing = true;
        self.error = None;
        cx.notify();
        let conn = self.conn.clone();
        let held = self.commit_session.clone();
        cx.spawn(async move |this, cx| {
            let (outcome, unsettled) = cx
                .background_executor()
                .spawn(async move {
                    let sid = match harbor_client::session_new(&conn) {
                        Ok(sid) => sid,
                        Err(message) => return (Committed::Refused(message), None),
                    };
                    *held.lock().unwrap_or_else(|p| p.into_inner()) = Some(sid.clone());
                    // Everything before COMMIT fails cleanly: the release
                    // below rolls the transaction back. COMMIT itself is
                    // the one request whose lost answer leaves the outcome
                    // unknown, since the server may have committed before the
                    // connection failed.
                    let result = match run_statements(&conn, &sid, &stmts) {
                        Err(message) => Committed::Refused(message),
                        Ok(()) => commit_verdict(harbor_client::exec_checked(
                            &conn,
                            "COMMIT",
                            None,
                            Some(&sid),
                        )),
                    };
                    // Releasing the session rolls back anything uncommitted,
                    // so a failed run can never half-land. A COMMIT that got
                    // no answer may still be running: its session is ended
                    // and seen to be over before the page is read, so the
                    // page shows the commit's outcome and not a moment
                    // before it.
                    // A session not seen to end is kept by name with the
                    // held set: nothing is judged while it may be running.
                    let unsettled = match &result {
                        Committed::InDoubt(_) => {
                            let ended =
                                harbor_client::session_end(&conn, &sid, std::time::Duration::from_secs(15));
                            (ended != harbor_client::Ended::Settled).then(|| sid.clone())
                        }
                        _ => {
                            harbor_client::session_release(&conn, &sid);
                            None
                        }
                    };
                    held.lock().unwrap_or_else(|p| p.into_inner()).take();
                    (result, unsettled)
                })
                .await;
            this.update(cx, |grid, cx| {
                match outcome {
                    Committed::Landed => {
                        // The values on screen ARE the committed truth —
                        // fold them into the display rows before the
                        // staged layer clears, so nothing reverts while
                        // the refetch is in flight. All the eye sees is
                        // the amber leaving. Deleted rows keep their
                        // ghosts until the refetch removes them for
                        // real (a ghost that briefly looked alive again
                        // would be its own artifact).
                        grid.table.update(cx, |state, cx| {
                            let d = state.delegate_mut();
                            let staged: Vec<_> = d.staged.drain().collect();
                            for ((row, col), text) in staged {
                                if let Some(cell) =
                                    d.rows.get_mut(row).and_then(|r| r.get_mut(col))
                                {
                                    *cell = text;
                                }
                            }
                            state.refresh(cx);
                            cx.notify();
                        });
                        if let Some(e) = &mut grid.edits {
                            e.clear();
                        }
                        // INSERT drafts have no fetched identity to fold
                        // into the existing page. Remove their synthetic
                        // rows now; the refetch below decides whether each
                        // committed row belongs on this page/filter.
                        grid.table.update(cx, |state, cx| {
                            let d = state.delegate_mut();
                            d.remove_drafts();
                            d.rebuild_cols();
                            state.clear_selection(cx);
                            let d = state.delegate_mut();
                            d.selection.clear();
                            d.active_cell = None;
                            state.refresh(cx);
                            cx.notify();
                        });
                        // No sync_staged here: it would also clear the
                        // delete ghosts. The refetch's own sync does.
                        // Fetch-first still holds: the page refetches so
                        // every row shows the database's truth —
                        // defaults filled, triggers applied — landing
                        // over pixels that already match it. The grid
                        // stays committing until that page lands: the
                        // rows on screen show the committed values under
                        // the identities they were fetched with, and an
                        // edit staged now would name a row by a key the
                        // commit may have changed or handed to another.
                        grid.post_commit = Some(PostCommit::Landed);
                        let page = grid.page;
                        grid.fetch_page_now(page, cx);
                        cx.emit(crate::app::CatalogRefreshRequested);
                    }
                    Committed::Refused(message) => {
                        grid.committing = false;
                        grid.error = Some(format!("{message} · edits kept"));
                        cx.emit(crate::app::CommitSettled);
                    }
                    Committed::InDoubt(message) => {
                        // The staged set is kept: if nothing landed it is
                        // still the work to send. The page is fetched so the
                        // user can see which, and when it arrives the set is
                        // held for review and the status line says why.
                        grid.post_commit = Some(PostCommit::InDoubt { message, unsettled });
                        let page = grid.page;
                        grid.fetch_page_now(page, cx);
                        cx.emit(crate::app::CatalogRefreshRequested);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// A refetch of the same page (post-commit) — unlike fetch_page this
    /// never skips on "already there".
    fn fetch_page_now(&mut self, page: usize, cx: &mut Context<Self>) {
        let size = self.page_size;
        self.fetch(
            PageReq { page, size, filter: FilterChange::Keep, recount: true },
            cx,
        );
    }

    /// Refresh the current Data page in place. A provisional editor value
    /// is first staged (or the refresh stops on validation failure), so a
    /// refresh can never silently discard what the user was typing.
    pub(crate) fn refresh_current(&mut self, cx: &mut Context<Self>) {
        if self.editor.is_some() && !self.confirm_and_move(0, 0, cx) {
            return;
        }
        self.fetch_page_now(self.page, cx);
    }

    /// Rebuild the column list after the row-number preference flips.
    fn sync_columns(&mut self, cx: &mut Context<Self>) {
        let want = prefs::get(cx).row_numbers;
        self.remap_columns(cx, |d| {
            if d.schema_cols.is_empty() || d.gutter == want {
                return false;
            }
            d.gutter = want;
            true
        });
    }
}

/// Make a row the selection's lead the dirty-aware way. The delegate
/// always records it — the ring and ⌘⌫ need a selected row — but the
/// library's selection (the blue row wash) is only requested for clean
/// rows: once a row carries staged changes, its amber or red owns the
/// story, and two washes fighting on one row read as confusion, not
/// state. A row not yet in the selection replaces it.
fn select_row(
    state: &mut TableState<GridDelegate>,
    row: usize,
    cx: &mut Context<TableState<GridDelegate>>,
) {
    state.delegate_mut().selection.lead_on(Some(row));
    if state.delegate().row_dirty(row) {
        state.clear_selection(cx);
        state.delegate_mut().selection.lead_on(Some(row));
        state.scroll_to_row(row, cx);
    } else {
        state.set_selected_row(row, cx);
    }
}

/// Route a click on a row through the selection grammar, then seat the
/// library on the new lead. Returns that lead — None when the click
/// emptied the selection, which the library learns about through
/// clear_selection. A click that deselects the ring's row takes the ring
/// with it, from the gutter and the body alike: a ring on a row the
/// selection no longer includes would lie about where the keys act.
fn click_row(
    state: &mut TableState<GridDelegate>,
    row: usize,
    kind: ClickKind,
    cx: &mut Context<TableState<GridDelegate>>,
) -> Option<usize> {
    let d = state.delegate_mut();
    let lead = d.selection.click(row, kind);
    if !d.selection.contains(row) && d.active_cell.is_some_and(|(r, _)| r == row) {
        d.active_cell = None;
    }
    match lead {
        Some(lead) => select_row(state, lead, cx),
        None => state.clear_selection(cx),
    }
    lead
}

/// What a click on a row means for the selection — the macOS list
/// grammar (Finder, NSTableView, TablePro's row gutter): plain replaces,
/// ⌘ toggles one row, ⇧ spans from the anchor. ⇧ wins when both are down.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ClickKind {
    Plain,
    Toggle,
    Extend,
}

impl ClickKind {
    fn of(m: &Modifiers) -> Self {
        if m.shift {
            Self::Extend
        } else if m.platform {
            Self::Toggle
        } else {
            Self::Plain
        }
    }
}

/// The rows a grid has selected. `rows` is every one of them; `lead` is
/// the one the ring, the inspector, and the library's own selection
/// follow; `anchor` is where the next ⇧-click spans from. All display
/// positions, like every selection index here (docs/DESIGN.md). Invariant:
/// the lead is in `rows`, and an empty selection has no lead.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
struct RowSelection {
    rows: std::collections::BTreeSet<usize>,
    lead: Option<usize>,
    anchor: Option<usize>,
}

impl RowSelection {
    /// Apply a click and return the new lead.
    fn click(&mut self, row: usize, kind: ClickKind) -> Option<usize> {
        match kind {
            ClickKind::Plain => {
                self.rows.clear();
                self.rows.insert(row);
                self.anchor = Some(row);
                self.lead = Some(row);
            }
            ClickKind::Toggle => {
                self.anchor = Some(row);
                if self.rows.remove(&row) {
                    // Deselecting the lead hands the role to the last
                    // remaining row; deselecting any other row leaves
                    // the lead where it was, so nothing scrolls.
                    if self.lead == Some(row) {
                        self.lead = self.rows.iter().next_back().copied();
                    }
                } else {
                    self.rows.insert(row);
                    self.lead = Some(row);
                }
            }
            ClickKind::Extend => {
                // The span REPLACES the selection rather than joining it,
                // the way Finder and TablePro read a ⇧-click; the anchor
                // stays put so a second ⇧-click re-spans from the same row.
                let anchor = self.anchor.unwrap_or(row);
                self.rows = (anchor.min(row)..=anchor.max(row)).collect();
                self.anchor = Some(anchor);
                self.lead = Some(row);
            }
        }
        self.lead
    }

    /// Adopt a lead chosen elsewhere — the library's own click, or a
    /// re-select after staging changed a row's wash. A lead already
    /// selected keeps the rest of the selection; any other row replaces
    /// it; None empties it.
    fn lead_on(&mut self, row: Option<usize>) {
        match row {
            Some(row) if self.rows.contains(&row) => self.lead = Some(row),
            Some(row) => {
                self.click(row, ClickKind::Plain);
            }
            None => self.clear(),
        }
    }

    fn clear(&mut self) {
        self.rows.clear();
        self.lead = None;
        self.anchor = None;
    }

    fn contains(&self, row: usize) -> bool {
        self.rows.contains(&row)
    }
}

impl GridDelegate {
    fn draft_count(&self) -> usize {
        self.draft_keys.len()
    }

    fn fetched_count(&self) -> usize {
        self.rows.len().saturating_sub(self.draft_count())
    }

    fn draft_key(&self, row: usize) -> Option<&str> {
        self.draft_keys.get(row).map(String::as_str)
    }

    fn remove_drafts(&mut self) {
        let count = self.draft_count();
        if count == 0 {
            return;
        }
        self.rows.drain(..count.min(self.rows.len()));
        self.identities.drain(..count.min(self.identities.len()));
        self.row_labels.drain(..count.min(self.row_labels.len()));
        self.draft_keys.clear();
        self.draft_cells.clear();
        self.staged = self
            .staged
            .drain()
            .filter_map(|((row, col), value)| {
                (row >= count).then_some(((row - count, col), value))
            })
            .collect();
        self.deleted = self
            .deleted
            .drain()
            .filter_map(|row| (row >= count).then_some(row - count))
            .collect();
        self.row_of = self
            .identities
            .iter()
            .enumerate()
            .map(|(ix, id)| (edits::key_of(id), ix))
            .collect();
    }

    /// A row carrying any staged change — the rows whose color already
    /// tells a story, so the selection wash stays off them.
    fn row_dirty(&self, row: usize) -> bool {
        row < self.draft_count()
            || self.deleted.contains(&row)
            || self.staged.keys().any(|(r, _)| *r == row)
    }

    /// Adopt a page's schema and rows — the one birth, shared by Grid::new
    /// and the first successful fetch of an error-born grid. The display
    /// names are derived here, once: three surfaces (headers, popover,
    /// inspector) read them per frame and must only bump SharedStrings.
    fn commit_schema(
        &mut self,
        page: harbor_client::QueryResult,
        base: usize,
        zoom: f32,
        pk_cols: &[String],
    ) {
        self.numeric =
            page.columns.iter().map(|c| numeric(&c.duckdb_type.to_uppercase())).collect();
        self.names = page
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| SharedString::from(c.name.clone().unwrap_or_else(|| format!("col{i}"))))
            .collect();
        self.pk_ix = pk_cols
            .iter()
            .filter_map(|k| self.names.iter().position(|n| n.as_ref() == k))
            .collect();
        // Identity requires the WHOLE key: a partial match would target
        // the wrong rows, so a key column missing from the result set
        // (impossible for SELECT *, but honesty is cheap) disables it.
        if self.pk_ix.len() != pk_cols.len() {
            self.pk_ix.clear();
        }
        self.schema_cols = page.columns;
        // Both are keyed by schema index, which means these columns only.
        self.hidden.clear();
        self.widths.clear();
        self.adopt_rows(page.rows, base);
        // The first page sizes the columns to their content; from here on
        // widths hold still (pages replace, fits don't).
        self.fit_widths(zoom);
    }

    /// Take a page's rows: capture each row's identity (the key columns'
    /// raw values) before display conversion, then derive the render-side
    /// strings and labels. The one door rows enter the delegate through.
    fn adopt_rows(&mut self, rows: Vec<Vec<Value>>, base: usize) {
        self.draft_keys.clear();
        self.draft_cells.clear();
        self.staged.clear();
        self.deleted.clear();
        self.identities = if self.pk_ix.is_empty() {
            Vec::new()
        } else {
            rows.iter()
                .map(|r| {
                    self.pk_ix
                        .iter()
                        .map(|&i| r.get(i).cloned().unwrap_or(Value::Null))
                        .collect()
                })
                .collect()
        };
        self.row_of = self
            .identities
            .iter()
            .enumerate()
            .map(|(ix, id)| (edits::key_of(id), ix))
            .collect();
        self.rows = display_rows(&rows);
        self.relabel(base);
    }

    /// The gutter's absolute row numbers, derived once per page commit —
    /// page 2 starts at 5,001 and the label says so without a per-frame
    /// format!. `base` is the page's first absolute row (page × size),
    /// which only Grid knows.
    fn relabel(&mut self, base: usize) {
        self.base = base;
        self.row_labels = (0..self.rows.len())
            .map(|r| SharedString::from((base + r + 1).to_string()))
            .collect();
    }

    /// Content-fit every visible column from the rows in hand, Sheets
    /// style. The value font is monospace, so a column's width is its
    /// longest cell's character count times the glyph advance — no text
    /// measurement pass. Fits land in `widths`, the same slot drag-resizes
    /// use: later rebuilds keep them, page flips never re-fit, and a drag
    /// still overrides a fit.
    fn fit_widths(&mut self, zoom: f32) {
        self.zoom = zoom;
        self.rebuild_cols();
        let fits: Vec<(usize, Pixels)> = self
            .visible
            .iter()
            .map(|&ix| (ix, self.fitted_width(ix, zoom)))
            .collect();
        self.widths.extend(fits);
        self.rebuild_cols();
    }

    /// Fit a single column (double-click on its header).
    fn fit_one(&mut self, schema_ix: usize, zoom: f32) {
        if schema_ix >= self.schema_cols.len() {
            return; // the header's usize::MAX sentinel for an unmapped column
        }
        let w = self.fitted_width(schema_ix, zoom);
        self.widths.insert(schema_ix, w);
        self.rebuild_cols();
    }

    /// One column's content-fit width. Menlo's advance is 1233/2048 em —
    /// CoreText's own number, linear in size, so character count times
    /// advance IS the text width, no measurement pass. The header is the
    /// proportional UI font, estimated generously. The extra covers the
    /// cell's own insets (PANE_INSET pad + 1px divider) and the editor's
    /// caret lookahead: the input scrolls whenever the caret comes within
    /// 10px of its right edge, so a fitted column must leave 10px past
    /// its content or a cell shifts the instant it opens for editing —
    /// view and edit must paint the same pixels. 16 is where the caret
    /// breathes: inside the editor's ring the text clears the left edge
    /// by 10px, and 16 gives the caret 12.5px on the right (16 − 2px
    /// ring − 1.5px caret) — a touch MORE than the left, deliberately: a
    /// 1.5px caret doesn't hold space the way a wall of glyphs does, so
    /// equal air reads tight beside it. Judged on screen at 14 (tight)
    /// and 16 (right).
    fn fitted_width(&self, schema_ix: usize, zoom: f32) -> Pixels {
        const CAP: usize = 60;
        let advance = CELL_TEXT * (1233. / 2048.) * zoom;
        let header_advance = HEADER_TEXT * (7. / 11.) * zoom;
        let mut chars = 4; // the NULL tag's footprint
        for row in &self.rows {
            if let Some(Some(s)) = row.get(schema_ix) {
                chars = chars.max(s.chars().count());
                if chars >= CAP {
                    break;
                }
            }
        }
        let name_len =
            self.schema_cols[schema_ix].name.as_deref().map_or(4, |n| n.chars().count());
        let content = chars.min(CAP) as f32 * advance;
        let header = name_len as f32 * header_advance;
        px((content.max(header) + PANE_INSET + 16.).clamp(60. * zoom, 460. * zoom))
    }

    /// Rebuild the display columns from the schema minus the hidden set
    /// (plus the gutter), refreshing the visible→schema map.
    fn rebuild_cols(&mut self) {
        self.visible = (self.identity as usize..self.schema_cols.len())
            .filter(|i| !self.hidden.contains(i))
            .collect();
        self.cols = build_columns(&self.names, &self.visible, self.gutter);
        let g = self.gutter as usize;
        // A column's width is its content's or the user's, never a draft
        // hint's: hints fit the width they are given, so starting an edit
        // or adding a row never moves a column.
        for (disp, &schema_ix) in self.visible.iter().enumerate() {
            if let Some(&w) = self.widths.get(&schema_ix) {
                self.cols[disp + g].width = w;
            }
        }
        if self.gutter {
            let last = (self.base + self.fetched_count()) as u64;
            self.cols[0].width = px(gutter_width(last));
        }
    }
}

impl TableDelegate for GridDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.cols.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> TableColumn {
        self.cols[col_ix].clone()
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let t = pal(cx);
        // The horizontal virtual_list sizes items by MEASURING the first
        // one, so an h_full cell resolves to its content height and the
        // dividers fall short of the row lines. Cells therefore take the
        // row height explicitly and draw their own bottom border; vertical
        // and horizontal lines meet at the corners.
        let p = prefs::get(cx);
        let row_h = p.table_size().table_row_height();
        // Column 0 is the row-number gutter: raised, muted, and a firmer
        // divider than the data cells (design.css `.grid td.num`).
        if self.gutter && col_ix == 0 {
            let is_draft = row_ix < self.draft_count();
            let row_deleted = self.deleted.contains(&row_ix);
            let row_dirty = is_draft
                || (!row_deleted && self.staged.keys().any(|(r, _)| *r == row_ix));
            return div()
                .h_flex()
                .relative()
                .w_full()
                .h(row_h)
                .items_center()
                .px_1p5()
                .bg(t.raised)
                // Select-all darkens the number rail a shade deeper than
                // the cells, the way Sheets treats its row headers.
                .when(self.all_selected, |d| d.bg(t.accent.opacity(0.16)))
                // A dirty row's number wears the row's own story — amber
                // for staged updates, red for a staged delete — so dirt
                // stays findable even with its column scrolled off-screen.
                .when(row_dirty, |d| d.bg(t.warn.opacity(0.18)))
                .when(row_deleted, |d| d.bg(t.bad.opacity(0.10)))
                .border_b_1()
                .border_color(t.grid_line)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |state, e: &MouseDownEvent, _, cx| {
                        state.delegate_mut().all_selected = false;
                        click_row(state, row_ix, ClickKind::of(&e.modifiers), cx);
                        // The Table's row select stops the event; the
                        // table's focus-on-click and the grid's body click
                        // above still need it.
                        cx.propagate();
                    }),
                )
                .child(
                    div()
                        .w_full()
                        .text_right()
                        .text_size(px(GUTTER_TEXT))
                        .font_family(value_font())
                        .text_color(if is_draft { t.warn } else { t.muted })
                        // Absolute position: page 2 starts at 5,001, and
                        // the number says so (plain digits, like Sheets).
                        .child(self.row_labels.get(row_ix).cloned().unwrap_or_default()),
                )
                // The gutter's divider strip, in the ONE grid-line
                // color, and the boundary's one owner: the Table draws
                // no fixed-columns edge of its own (fixed_cols_border),
                // which would sit 1px beside this one, half-occluded by
                // the scrolling cells.
                .child(div().absolute().right_0().top_0().bottom_0().w(px(1.)).bg(t.grid_line))
                .into_any_element();
        }
        // Display position -> schema index, through the visible map.
        let Some(data_col) = self.visible.get(col_ix - self.gutter as usize).copied() else {
            return div().into_any_element();
        };
        // An open editor replaces the cell's content outright — the
        // editor surface IS the state (no tint underneath). It wears the
        // active-cell ring so the eye never has to relocate.
        if self.editing == Some((row_ix, data_col)) {
            if let Some(input) = self.editor_input.clone() {
                return div()
                    .h_flex()
                    .relative()
                    .w_full()
                    .h(row_h)
                    .items_center()
                    .border_r_1()
                    .border_b_1()
                    .border_color(t.grid_line)
                    .child(
                        div()
                            .absolute()
                            .left(px(-PANE_INSET))
                            .right_0()
                            .top_0()
                            .bottom_0()
                            .bg(t.surface)
                            .border_2()
                            .border_color(t.accent),
                    )
                    .child(
                        div().w_full().child(
                            gpui_kit::component::input::Input::new(&input)
                                .appearance(false)
                                // Zero the input's built-in insets, both
                                // sides. Left pins the caret exactly on
                                // the column's text axis; right keeps the
                                // editor's text area at least as wide as
                                // the display cell's, so opening the
                                // editor on a fitted column never scrolls
                                // the value — view and edit paint the
                                // same pixels. (The input's Medium inset
                                // is 12px a side; a fitted column has
                                // only ~9px of slack, and the caret needs
                                // 1.5px more — the old right inset made
                                // every fitted cell start edit shifted.)
                                .pl(px(0.))
                                .pr(px(0.))
                                .text_size(px(CELL_TEXT * p.zoom_factor()))
                                .font_family(value_font()),
                        ),
                    )
                    // The column card (⌘T), floating just under the cell for as
                    // long as it is edited, so the value being edited stays
                    // in view: a zero-size box seats it below the cell's
                    // bottom edge, and it is deferred so it paints over the
                    // rows below and escapes the table's clip, snapped to
                    // stay inside the window.
                    .when(p.column_cards, |d| d.child(
                        div().absolute().left(px(-PANE_INSET)).top(row_h + px(4.)).child(
                            deferred(
                                anchored()
                                    .snap_to_window_with_margin(px(8.))
                                    .child(
                                        column_card(
                                            t,
                                            p.zoom_factor(),
                                            self.names.get(data_col).cloned().unwrap_or_default(),
                                            self.col_types.get(data_col).cloned().unwrap_or_default(),
                                            &self
                                                .draft_hints
                                                .get(data_col)
                                                .cloned()
                                                .unwrap_or(DraftHint::Null),
                                            self.pick.clone(),
                                        )
                                        .occlude(),
                                    ),
                            )
                            .with_priority(1),
                        ),
                    ))
                    .into_any_element();
            }
        }
        let right = p.right_align && self.numeric.get(data_col).copied().unwrap_or(false);
        // The staged layer overrides the fetched value: a confirmed edit
        // shows its new text (or NULL) under a soft accent tint until ⌘S
        // makes it the database's truth.
        let is_draft = row_ix < self.draft_count();
        let draft_explicit = is_draft && self.draft_cells.contains_key(&(row_ix, data_col));
        let staged = (!is_draft).then(|| self.staged.get(&(row_ix, data_col)).cloned()).flatten();
        let is_staged = is_draft || staged.is_some();
        let value = match staged {
            Some(v) => Some(v),
            None => self.rows.get(row_ix).and_then(|r| r.get(data_col)).cloned(),
        };
        let is_deleted = self.deleted.contains(&row_ix);
        // The column paddings are zeroed (build_columns), so this div owns
        // the cell: full height, the vertical divider on its right edge,
        // and its own text inset.
        let active = self.active_cell == Some((row_ix, data_col));
        let cell = div()
            .h_flex()
            .relative()
            .w_full()
            .h(row_h)
            .items_center()
            .pr_2()
            .border_r_1()
            .border_b_1()
            .border_color(t.grid_line)
            // The corner's select-all, made visible (Sheets: every cell
            // highlights until an ordinary click disarms it). The tint is
            // a full-bleed layer reaching back across the column wrapper's
            // PANE_INSET of left padding — on the cell box alone, the
            // padding shows through as white stripes between columns.
            .when(self.all_selected, |d| {
                d.child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(px(-PANE_INSET))
                        .right_0()
                        .bg(t.accent.opacity(0.08)),
                )
            })
            // Staged-but-uncommitted: a soft amber wash — "modified, not
            // yet saved," the color that is neither the accent (where you
            // are) nor the danger red (what you are destroying). Same
            // full-bleed layer trick as select-all, same reason.
            .when(is_staged && !is_deleted, |d| {
                d.child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(px(-PANE_INSET))
                        .right_0()
                        .bg(t.warn.opacity(0.16)),
                )
            })
            // A staged DELETE ghosts the whole row: a danger wash here,
            // strikethrough on the text below. Reversible until commit.
            .when(is_deleted, |d| {
                d.child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(px(-PANE_INSET))
                        .right_0()
                        .bg(t.bad.opacity(0.07)),
                )
            })
            // The cell starts PANE_INSET in (wrapper padding), so its
            // bottom border leaves a notch there. Every row but the LAST
            // hides it under the tr's full-width border, which the Table
            // skips on the last row; this strip patches the notch on the
            // border's own pixel (bottom -1).
            .child(
                div()
                    .absolute()
                    .left(px(-PANE_INSET))
                    .w(px(PANE_INSET))
                    .bottom(px(-1.))
                    .h(px(1.))
                    .bg(t.grid_line),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |state, e: &MouseDownEvent, _, cx| {
                    // Row and cell select together on mouse DOWN — the
                    // Table's own row selection waits for the click (mouse
                    // up), which reads as lag next to the ring.
                    let d = state.delegate_mut();
                    d.all_selected = false;
                    d.tab_anchor = None;
                    let lead = click_row(state, row_ix, ClickKind::of(&e.modifiers), cx);
                    // The ring sits on the clicked cell while its row is
                    // selected; click_row has already dropped it if the
                    // click deselected the row.
                    if lead == Some(row_ix) {
                        state.delegate_mut().active_cell = Some((row_ix, data_col));
                    }
                    cx.notify();
                    // The Table's row select stops the event; the table's
                    // focus-on-click and the grid's body click above (the
                    // double-click that opens the editor) still need it.
                    cx.propagate();
                }),
            )
            .when(active, |d| {
                d.child(
                    // Sheets' active-cell ring. The wrapper owns PANE_INSET
                    // of left padding, so the ring reaches back that far to
                    // sit on the cell's true grid box.
                    div()
                        .absolute()
                        .left(px(-PANE_INSET))
                        .right_0()
                        .top_0()
                        .bottom_0()
                        .border_2()
                        .border_color(t.accent),
                )
            });
        if is_draft && !draft_explicit {
            // An untouched draft cell is blank: the database fills it — a
            // default, a sequence, a generated value, or NULL — and the
            // column card on hover says which. Only a value the row cannot
            // commit without shows: a soft red REQUIRED tag, in the NULL
            // tag's style, or the same tag holding only "!" where the
            // column is too narrow for the word. A generated cell, which
            // takes no typing, is faintly dimmed.
            let hint = self.draft_hints.get(data_col).cloned().unwrap_or(DraftHint::Null);
            let z = p.zoom_factor();
            let room = self.cols.get(col_ix).map_or(px(0.), |c| c.width) - px(PANE_INSET + 8.);
            let required = (hint == DraftHint::Required).then(|| {
                div()
                    .flex_none()
                    .px(px(5.))
                    .rounded(px(4.))
                    .bg(t.bad.opacity(0.12))
                    .text_size(px(TAG_TEXT * z))
                    .font_family(ui_font())
                    .text_color(t.bad)
                    .child(if tag_width("REQUIRED", z) <= room { "REQUIRED" } else { "!" })
            });
            let generated = matches!(hint, DraftHint::Generated(_));
            let name = self.names.get(data_col).cloned().unwrap_or_default();
            let ty = self.col_types.get(data_col).cloned().unwrap_or_default();
            let cards = p.column_cards;
            return cell
                .id(SharedString::from(format!("draft-hint-{row_ix}-{data_col}")))
                .when(right, |d| d.justify_end())
                .when(generated, |d| {
                    d.child(
                        div()
                            .absolute()
                            .left(px(-PANE_INSET))
                            .right_0()
                            .top_0()
                            .bottom_0()
                            .bg(t.muted.opacity(0.08)),
                    )
                })
                .children(required)
                .when(cards, |d| d.tooltip(move |window, cx| {
                    let (name, ty, hint) = (name.clone(), ty.clone(), hint.clone());
                    Tooltip::element(move |_, _| column_card(t, z, name.clone(), ty.clone(), &hint, None))
                        .build(window, cx)
                }))
                .into_any_element();
        }
        match value {
            None | Some(None) => {
                if !p.null_tags {
                    return cell.into_any_element();
                }
                cell.when(right, |d| d.justify_end())
                    .child(
                        div()
                            .flex_none()
                            .px(px(5.))
                            .rounded(px(4.))
                            .bg(t.pill.opacity(0.55))
                            .text_size(px(TAG_TEXT * p.zoom_factor()))
                            .font_family(ui_font())
                            .text_color(t.muted.opacity(0.65))
                            .child("NULL"),
                    )
                    .into_any_element()
            }
            Some(Some(text)) => {
                if self.pill_cols.contains(&data_col) {
                    // " · "-joined tags as pills; PK wears the accent,
                    // the rest the NULL tag's muted chassis.
                    let mut row = cell.h_flex().gap_1();
                    for tag in text.split(" \u{00b7} ").filter(|s| !s.is_empty()) {
                        let accent = tag == "PK";
                        row = row.child(
                            div()
                                .flex_none()
                                .px(px(5.))
                                .rounded(px(4.))
                                .bg(if accent {
                                    t.accent.opacity(0.15)
                                } else {
                                    t.pill.opacity(0.55)
                                })
                                .text_size(px(TAG_TEXT * p.zoom_factor()))
                                .font_family(ui_font())
                                .text_color(if accent {
                                    t.accent
                                } else {
                                    t.muted.opacity(0.85)
                                })
                                .child(tag.to_string()),
                        );
                    }
                    return row.into_any_element();
                }
                let text = text.clone();
                cell.child(
                    div()
                        .w_full()
                        .truncate()
                        .text_size(px(CELL_TEXT * p.zoom_factor()))
                        .font_family(value_font())
                        .text_color(if is_deleted { t.muted } else { t.text })
                        .when(is_deleted, |d| d.line_through())
                        .when(right, |d| d.text_right())
                        .child(text),
                )
                .into_any_element()
            }
        }
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let t = pal(cx);
        let p = prefs::get(cx);
        // The th wrapper compensates zeroed column paddings with the
        // active size's cell padding on the right (4px at XSmall, 12px at
        // Large), so a strip at right_0 lands that far inboard of the body
        // cells' dividers. right(-comp) puts it back on the true column
        // edge at every zoom level; the header row is not clipped there
        // (the clip is the padded cell box). The strip also spans the full
        // header height, where the built-in resize-handle line falls short
        // of the top and bottom.
        let comp = p.table_size().table_cell_padding().right;
        let edge = move |color: Hsla| {
            div().absolute().right(-comp).top_0().bottom_0().w(px(1.)).bg(color)
        };
        // Explicit height, like the body cells: the th sits in a chain
        // that resolves h_full to content height, so the edge strips fall
        // short of the header's top and bottom without it.
        let row_h = p.table_size().table_row_height();
        if self.gutter && col_ix == 0 {
            // Mirror the gutter's body cells (same flex centering, inset,
            // and font), so "#" sits on the numbers' baseline and shares
            // their right edge: the td inset is 6px, the wrapper already
            // padded `comp` of it, and the margin supplies the difference
            // (negative when the wrapper alone overshoots).
            return div()
                .relative()
                .h_flex()
                .items_center()
                .w_full()
                .h(row_h)
                .pl(px(6.))
                // The Sheets corner: clicking "#" highlights every cell,
                // arming the divider double-click below to fit the whole
                // table instead of one column.
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|state, _: &MouseDownEvent, _, cx| {
                        state.delegate_mut().all_selected = true;
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .w_full()
                        .text_right()
                        .mr(px(6.) - comp)
                        .text_size(px(GUTTER_TEXT))
                        .font_family(value_font())
                        .text_color(t.muted)
                        .child("#"),
                )
                .child(edge(t.grid_line))
                .into_any_element();
        }
        let data_col =
            self.visible.get(col_ix - self.gutter as usize).copied().unwrap_or(usize::MAX);
        let right = p.right_align && self.numeric.get(data_col).copied().unwrap_or(false);
        // Left-aligned headers line up with values on the shared wrapper
        // inset by construction. A right-aligned header aims for the cell
        // text's edge, PANE_INSET + 1px divider in: the wrapper already
        // padded `comp` of it, and the margin supplies the difference
        // (negative when the wrapper alone overshoots).
        div()
            .relative()
            .h_flex()
            .items_center()
            .w_full()
            .h(row_h)
            .text_size(px(HEADER_TEXT * p.zoom_factor()))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(t.text)
            .child(
                div()
                    .w_full()
                    .truncate()
                    .when(right, |d| d.text_right().mr(px(PANE_INSET + 1.) - comp))
                    .child(self.cols[col_ix].name.clone()),
            )
            .child(edge(t.grid_line))
            .into_any_element()
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        let t = pal(cx);
        // The selection tint paints here, UNDER the cells and inside the
        // row's own bottom border, so both edges of the selected row are
        // ordinary dividers. It is a child, not the row's background: the
        // Table paints its hover wash and its lead-row highlight on the
        // row itself, after this element is built, and a child sits above
        // both — every selected row keeps one tint under the pointer, the
        // lead included.
        div()
            .id(("row", row_ix))
            .relative()
            .when(self.selection.contains(row_ix), |d| {
                d.child(div().absolute().inset_0().bg(t.row_active))
            })
    }

    /// The Table's loading placeholder is for a grid with nothing to show
    /// yet: no schema, no rows. A refresh of a grid on screen — an empty
    /// table included — keeps showing what it has until the new page
    /// lands, so the refresh never flashes the placeholder.
    fn loading(&self, _: &App) -> bool {
        self.loading && self.rows.is_empty() && self.schema_cols.is_empty()
    }

}

impl Grid {
    /// The table element itself — the virtualized Table plus the
    /// header-row hit-test canvas — shared by the Data arm and the
    /// embedded (query-results) render.
    fn table_body(&mut self, cx: &mut Context<Self>) -> Div {
        // The table sits in a wrapper we own, whose bounds a canvas
        // records each frame: double-clicks anywhere in the header row
        // hit-test against the column boundaries geometrically (widths +
        // horizontal scroll), so the fit gesture works ON the divider
        // line itself — the 4px either side the library's drag handle
        // occludes included, because ancestors still hear what it doesn't
        // consume.
        let bounds_store = self.table_bounds.clone();
        let header_h = prefs::get(cx).table_size().table_row_height();
        div()
            .relative()
            .size_full()
            // The last grid line is the boundary's problem, not ours:
            // whatever sits below a grid's bottom edge (the footer's
            // border_t, the Structure divider's handle) draws its own
            // 1px line, so a flush table bottom would stack on it — 2px
            // under the columns, 1px past the table's right edge. The
            // table hangs one px below this clip instead: a bottom edge
            // AT the clip sheds its border into the hidden px, while a
            // short table ending mid-pane keeps its closing border.
            .overflow_hidden()
            // Bubble-phase: the cell's own mouse-down (first click of the
            // pair) has already set active_cell, so a double-click opens
            // the editor right there.
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_body_click))
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .bottom(px(-1.))
                    .child(
                        DataTable::new(&self.table)
                            .bordered(false)
                            .fixed_cols_border(false)
                            .with_size(prefs::get(cx).table_size()),
                    ),
            )
            // Painted AFTER the table, so nothing the table occludes
            // (drag handles, scroll containers) can eclipse it — while,
            // carrying no occlusion of its own, everything beneath still
            // hears its events (dragging on the line keeps working). The
            // canvas rides INSIDE the strip: the strip's own bounds are
            // the header-row frame the hit-test needs.
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .h(header_h)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(Self::divider_double_click),
                    )
                    .child(
                        canvas(move |b, _, _| bounds_store.set(b), |_, _, _, _| {})
                            .size_full(),
                    ),
            )
    }
}

impl Render for Grid {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = pal(cx);
        let p = prefs::get(cx);
        // Self-healing gutter: display prefs are global and set once (the
        // strip's `#` tile), but EVERY grid honors them — a grid whose
        // columns predate the flip (the embedded results grid, say)
        // rebuilds itself on its next paint.
        let stale_gutter = {
            let d = self.table.read(cx).delegate();
            !d.schema_cols.is_empty() && d.gutter != p.row_numbers
        };
        if stale_gutter {
            self.sync_columns(cx);
        }
        // A closed editor hands focus back to the table here — render is
        // where a &mut Window exists (the InputEvent subscription has
        // none), so the flag set at close time is consumed one frame on.
        if self.needs_focus {
            self.needs_focus = false;
            window.focus(&self.table.focus_handle(cx), cx);
        }
        // The header chase: this frame's body paint will apply the
        // pending horizontal scroll AFTER the header has painted, so
        // the scrolling frame shows a stale header. A notify placed NOW
        // would be swallowed — the table renders later this same frame
        // and clears its dirty mark — so the request rides on_next_frame,
        // past this frame's paint. One frame, one flag, no loop.
        if self.header_chase {
            self.header_chase = false;
            let table = self.table.clone();
            window.on_next_frame(move |_, cx| {
                table.update(cx, |_, cx| cx.notify());
            });
        }
        // An editor whose column just got hidden would be invisible but
        // still focused — cancel it (lossless, like any Esc).
        if let Some(ed) = &self.editor {
            if !self.table.read(cx).delegate().visible.contains(&ed.col) {
                self.cancel_edit(cx);
            }
        }
        let view = p.view;
        // The title band names what fills the pane. Structure and Data
        // are about the selected table, so they wear its name; Query is
        // about whatever you ask — the berth-scoped scratchpad — so it
        // wears its own (the sidebar selection would be a lie there).
        let title = if view == ViewMode::Query {
            "Query".to_string()
        } else {
            self.title.clone()
        };
        let error = self.error.clone();
        let editing_cell = self.editor.is_some();
        // Embedded (the Query view's results pane): body only. The host
        // owns the chrome — its editor above, the app footer below,
        // which reads this grid's stats and pager state through it. The
        // ViewMode switch below is the HOST grid's concern; an embedded
        // grid is always its table.
        if self.embedded {
            // The inspector rides query results exactly as it rides the
            // Data grid — row_kv is delegate data, and a result row is
            // as inspectable as a table row. The Query gate keeps the
            // Structure view's embedded columns-grid out of it.
            let inspector = (p.inspector && p.view == ViewMode::Query)
                .then(|| self.inspector(cx).into_any_element());
            let body = {
                let table_el = self.table_body(cx);
                let body = div().flex_1().min_h_0().w_full();
                match inspector {
                    Some(pane) => body.child(
                        h_resizable("query-split")
                            .with_state(&self.resize)
                            .child(
                                resizable_panel()
                                    .child(div().size_full().child(table_el)),
                            )
                            .child(
                                resizable_panel()
                                    .size(px(p.inspector_width))
                                    .size_range(
                                        px(prefs::INSPECTOR_MIN)
                                            ..px(prefs::INSPECTOR_MAX),
                                    )
                                    // Furniture, like the sidebar: only
                                    // a drag changes this width.
                                    .fixed()
                                    .child(pane),
                            ),
                    ),
                    None => body.h_flex().child(
                        div().flex_1().min_w_0().h_full().child(table_el),
                    ),
                }
            };
            return div()
                .size_full()
                .min_w_0()
                .v_flex()
                // The editing keymap is inert without Edits, but the
                // navigation keys (page-step, ring moves' Escape) ride
                // the same handler.
                .on_key_down(cx.listener(Self::on_key))
                .when_some(error, |d, message| {
                    // A page-flip fetch that failed: the verdict shows
                    // here, the prior page stays.
                    d.child(
                        div()
                            .px_3()
                            .py_2()
                            .flex_none()
                            .text_xs()
                            .text_color(t.bad)
                            .child(message),
                    )
                })
                .child(body)
                .into_any_element();
        }
        // The inspector slots in BESIDE the table, below the header strip —
        // the title/toggle row keeps the full width, so opening the panel
        // never shifts it. It is row-level: here it accompanies Data; in
        // the Query view the EMBEDDED results grid carries its own
        // (the branch above).
        let inspector = (p.inspector && p.view == ViewMode::Data)
            .then(|| self.inspector(cx).into_any_element());
        div()
            .size_full()
            .min_w_0()
            .v_flex()
            // The whole editing keymap rides the pane, on the bubble path
            // from wherever focus is — the table or an open cell editor.
            .on_key_down(cx.listener(Self::on_key))
            // gpui-component binds Tab inside Input to indentation
            // actions. A single-line cell editor has nothing to indent,
            // so intercept those actions here and give Tab its grid
            // meaning: confirm, then move to the adjacent visible cell.
            .when(editing_cell, |d| {
                d.on_action(cx.listener(Self::on_editor_tab))
                    .on_action(cx.listener(Self::on_editor_shift_tab))
            })
            .child(
                div()
                    .h_flex()
                    .h_8()
                    .relative()
                    // The strip spans the pane in every view: its canvas
                    // records the width the Structure view needs BEFORE
                    // the DDL's first paint (see pane_width).
                    .child(
                        div().absolute().inset_0().child(
                            canvas(
                                move |b, _, _| {
                                    crate::structure::record_pane_width(b.size.width)
                                },
                                |_, _, _, _| {},
                            )
                            .size_full(),
                        ),
                    )
                    // Left inset matches the grid text (PANE_INSET cell
                    // padding), so the title sits flush over the first
                    // column.
                    .pl(px(PANE_INSET))
                    .pr_3()
                    .gap_3()
                    .flex_none()
                    .items_center()
                    // A shade beyond raised: the column-header row below
                    // is raised, and two identical bands would merge.
                    .bg(t.strip)
                    .border_b_1()
                    // The grid's top frame line, so it reads the slot —
                    // the title strip is chrome but this edge is the
                    // grid's (the red audit's "not red", 2026-09-01).
                    .border_color(t.grid_line)
                    .child(
                        // Semibold like the design proof's breadcrumb table
                        // name (`.crumb b`, weight 600).
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(t.text)
                            .truncate()
                            .child(title),
                    )
                    // The display toggles ride every view that shows
                    // rows — Data and Query alike (prefs are global, set
                    // once, honored by every grid); the Structure view
                    // drops them. The inspector glyph stays Data-only:
                    // its panel is row-level.
                    .when(matches!(view, ViewMode::Data | ViewMode::Query), |d| d.child(
                        // Recessed track, macOS-toolbar style: a subtle
                        // inset container; flat icon tiles with a 2px gap
                        // (edges never touch); the ON state is an
                        // accent-tinted fill. These are independent
                        // toggles, so no segment ever "wins" the track.
                        div()
                            .h_flex()
                            .flex_none()
                            .gap(px(2.))
                            .p(px(2.))
                            .rounded(px(6.))
                            // Surface track on the raised strip, the same
                            // relationship the footer seg has to its bar.
                            .bg(t.surface)
                            .border_1()
                            .border_color(t.pill)
                            .child(toggle_tile(
                                "toggle-rows",
                                "#",
                                "Show row numbers (\u{2318}7 or \u{2325}7)",
                                p.row_numbers,
                                t,
                                cx.listener(|_, _, _, cx| {
                                    // No explicit sync: the toggle
                                    // refreshes every window and each
                                    // grid self-heals its gutter at the
                                    // top of its own render.
                                    prefs::toggle(cx, |p| p.row_numbers = !p.row_numbers);
                                }),
                            ))
                            .child(toggle_tile(
                                "toggle-align",
                                "\u{21e5}",
                                "Right-align numeric columns (\u{2318}8 or \u{2325}8)",
                                p.right_align,
                                t,
                                cx.listener(|_, _, _, cx| {
                                    prefs::toggle(cx, |p| p.right_align = !p.right_align);
                                }),
                            ))
                            .child(toggle_tile(
                                "toggle-nulls",
                                "\u{2205}",
                                "Show NULL tags (\u{2318}9 or \u{2325}9)",
                                p.null_tags,
                                t,
                                cx.listener(|_, _, _, cx| {
                                    prefs::toggle(cx, |p| p.null_tags = !p.null_tags);
                                }),
                            )),
                    ))
                    .when(matches!(view, ViewMode::Data | ViewMode::Query), |d| d.child(
                        // The inspector's panel glyph (Finder/Xcode
                        // convention), right of the lozenge.
                        icon_tile("toggle-inspector", 22., true, t)
                            .text_color(if p.inspector { t.accent } else { t.muted })
                            .tooltip(|window, cx| {
                                Tooltip::new("Show inspector (\u{2318}I)").build(window, cx)
                            })
                            .on_click(cx.listener(|_, _, _, cx| {
                                prefs::toggle(cx, |p| p.inspector = !p.inspector);
                            }))
                            .child(
                                gpui_kit::component::Icon::new(
                                    gpui_kit::component::IconName::PanelRight,
                                )
                                .size_4(),
                            ),
                    )),
            )
            .when(view == ViewMode::Data, |d| {
                // The raw-SQL filter strip (DESIGN.md "Bottom bar"): one
                // WHERE input, applied on Enter through the same
                // fetch-first swap as everything else.
                d.when_some(self.filter_input.clone(), |d, input| {
                    d.child(
                        div()
                            .h_flex()
                            .flex_none()
                            .items_center()
                            .gap_2()
                            .pl(px(PANE_INSET))
                            .pr_2()
                            .py_1()
                            .bg(t.raised)
                            .border_b_1()
                            .border_color(t.grid_line)
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(11.))
                                    .font_family(value_font())
                                    .text_color(t.muted)
                                    .child("WHERE"),
                            )
                            .child(
                                div().flex_1().child(
                                    gpui_kit::component::input::Input::new(&input)
                                        .xsmall()
                                        .cleanable(true),
                                ),
                            ),
                    )
                })
            })
            .when_some(error, |d, message| {
                d.child(
                    div()
                        .px_3()
                        .py_2()
                        .flex_none()
                        .text_xs()
                        .text_color(t.bad)
                        .child(message),
                )
            })
            .child(match view {
                ViewMode::Data => {
                    let table_el = self.table_body(cx);
                    let body = div().flex_1().min_h_0().w_full();
                    match inspector {
                        // With the inspector open, the two panes share a
                        // draggable divider; the saved width seeds it.
                        Some(pane) => body.child(
                            h_resizable("data-split")
                                .with_state(&self.resize)
                                .child(
                                    resizable_panel()
                                        .child(div().size_full().child(table_el)),
                                )
                                .child(
                                    resizable_panel()
                                        .size(px(p.inspector_width))
                                        .size_range(
                                            px(prefs::INSPECTOR_MIN)..px(prefs::INSPECTOR_MAX),
                                        )
                                        // Furniture, like the sidebar.
                                        .fixed()
                                        .child(pane),
                                ),
                        ),
                        None => body.h_flex().child(
                            div().flex_1().min_w_0().h_full().child(table_el),
                        ),
                    }
                    .into_any_element()
                }
                ViewMode::Structure => div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(self.structure_view(cx))
                    .into_any_element(),
                ViewMode::Query => {
                    let body = div().flex_1().min_h_0().w_full();
                    match self.query_view.clone() {
                        Some(view) => body.child(view),
                        None => body
                            .v_flex()
                            .items_center()
                            .justify_center()
                            .child(div().text_sm().text_color(t.muted).child(
                                "Select a table once to open the query scratchpad.",
                            )),
                    }
                    .into_any_element()
                }
            })
            .child(self.footer(cx))
            .into_any_element()
    }
}

type InsertMetadata = (
    Vec<bool>,
    Vec<Option<String>>,
    Vec<bool>,
    Vec<DraftHint>,
    Vec<Option<String>>,
);

/// Align catalog insert capabilities to the result schema. The hidden
/// rowid has no catalog column and therefore receives the harmless NULL
/// defaults; users never see or insert it.
fn insert_metadata(
    names: &[SharedString],
    structure: Option<&crate::structure::TableStructure>,
) -> InsertMetadata {
    let cols: Vec<_> = names
        .iter()
        .map(|name| {
            structure.and_then(|s| s.cols.iter().find(|c| c.name == name.as_ref()))
        })
        .collect();
    let not_null: Vec<bool> = cols.iter().map(|c| c.is_some_and(|c| c.notnull)).collect();
    let defaults: Vec<Option<String>> =
        cols.iter().map(|c| c.and_then(|c| c.dflt.clone())).collect();
    let generated: Vec<bool> = cols.iter().map(|c| c.is_some_and(|c| c.generated)).collect();
    let hints = cols
        .iter()
        .enumerate()
        .map(|(col, c)| {
            if generated[col] {
                DraftHint::Generated(
                    c.and_then(|c| c.generation_expression.clone()).unwrap_or_default().into(),
                )
            } else if let Some(expr) = &defaults[col] {
                DraftHint::Default(expr.clone().into())
            } else if not_null[col] {
                DraftHint::Required
            } else {
                DraftHint::Null
            }
        })
        .collect();
    let types = cols.iter().map(|c| c.map(|c| c.ty.clone())).collect();
    (not_null, defaults, generated, hints, types)
}

/// The column card's types: the catalog's declaration where it has one,
/// else the type the result schema carries.
fn col_types(catalog: Vec<Option<String>>, schema: &[wire::Column]) -> Vec<SharedString> {
    catalog
        .into_iter()
        .zip(schema)
        .map(|(ty, col)| ty.unwrap_or_else(|| col.duckdb_type.clone()).into())
        .collect()
}

/// A column's rules at a glance: its name and type, what an untouched
/// cell becomes (its default, its generation, `required`, or nullable),
/// and an ENUM's values. Shown under a cell while it is edited, where
/// `pick` makes each value a click that fills the editor, and as the
/// tooltip of a draft row's placeholder.
fn column_card(
    t: crate::theme::Pal,
    z: f32,
    name: SharedString,
    ty: SharedString,
    hint: &DraftHint,
    pick: Option<PickValue>,
) -> Div {
    let choices = edits::enum_values(&ty);
    let ty_label: SharedString = if choices.is_some() { "ENUM".into() } else { ty };
    let small = px(TAG_TEXT * z + 1.);
    let rule = match hint {
        DraftHint::Generated(expr) if !expr.is_empty() => {
            div().text_color(t.muted).child(SharedString::from(format!("generated = {expr}")))
        }
        DraftHint::Generated(_) => div().text_color(t.muted).child("generated"),
        DraftHint::Default(expr) => {
            div().text_color(t.muted).child(SharedString::from(format!("default {expr}")))
        }
        DraftHint::Required => div()
            .h_flex()
            .gap_1()
            .child(div().text_color(t.bad).child("required"))
            .child(div().text_color(t.muted).child("· NOT NULL, no default")),
        DraftHint::Null => div().text_color(t.muted).child("nullable"),
    };
    div()
        .v_flex()
        .gap(px(3.))
        .max_w(px(340. * z))
        .px(px(8.))
        .py(px(6.))
        .rounded(px(6.))
        .border_1()
        .border_color(t.border)
        .bg(t.surface)
        .shadow_md()
        .text_size(small)
        .font_family(value_font())
        .child(
            div()
                .h_flex()
                .gap_2()
                .child(
                    div()
                        .font_family(ui_font())
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(t.text)
                        .child(name),
                )
                .child(div().text_color(t.muted).child(ty_label)),
        )
        .child(rule)
        .when_some(choices, |card, choices| {
            card.child(
                div()
                    .h_flex()
                    .flex_wrap()
                    .gap_1()
                    .child(div().text_color(t.muted).child("one of"))
                    .children(choices.into_iter().enumerate().map(|(ix, value)| {
                        let chip = div()
                            .id(("enum-choice", ix))
                            .px(px(5.))
                            .rounded(px(4.))
                            .bg(t.pill.opacity(0.55))
                            .text_color(t.text)
                            .child(value.clone());
                        match pick.clone() {
                            Some(pick) => chip
                                .cursor_pointer()
                                .hover(|d| d.bg(t.accent.opacity(0.18)))
                                .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                                    cx.stop_propagation();
                                    pick(value.clone().into(), window, cx);
                                }),
                            None => chip,
                        }
                    })),
            )
        })
}

/// How a commit ended.
#[derive(Debug, PartialEq)]
enum Committed {
    /// Every statement ran and Harbor acknowledged the COMMIT.
    Landed,
    /// Nothing landed: a statement failed or was refused, and releasing the
    /// session rolled the transaction back. The reason.
    Refused(String),
    /// The COMMIT was sent and no verdict came back. The reason.
    InDoubt(String),
}

/// What the page fetch after a commit settles when it lands.
#[derive(Debug, PartialEq)]
enum PostCommit {
    /// The commit landed and the staged set is cleared.
    Landed,
    /// The COMMIT got no answer; the staged set is kept, and held.
    /// `unsettled` is the commit's session when it was not seen to be over
    /// before the page was read: the commit may still be running.
    InDoubt { message: String, unsettled: Option<String> },
}

/// What the fetch a commit was waiting on settles before the page is drawn.
/// A set whose commit got no answer is held, with the session it may still
/// be running on and whether the database was `read` after it; held, it is
/// not drawn (`Edits::projection`). Returns whether a commit that landed is
/// left with a page that was not read after it.
fn settle_fetch(after: Option<&PostCommit>, read: bool, edits: Option<&mut Edits>) -> bool {
    match (after, edits) {
        (Some(PostCommit::InDoubt { unsettled, .. }), Some(edits)) => {
            edits.mark_in_doubt(unsettled.clone(), read);
            false
        }
        (Some(PostCommit::Landed), _) => !read,
        _ => false,
    }
}

/// Why a verdict on a held set is not taken, if it is not. Neither verdict
/// is taken while the commit may still be running or the database has not
/// been read since it ended: the page is what the set is judged against, and
/// a set staged again beside a commit still running could land twice. A set
/// is not staged again against columns the table does not have; dropping it
/// is the way out there, and is taken.
fn verdict_refusal(landed: bool, reshaped: bool, unjudged: Option<edits::Unjudged>) -> Option<&'static str> {
    match unjudged {
        // On changed columns the one verdict offered is the discard.
        Some(edits::Unjudged::Running) if reshaped => Some(
            "the commit that sent these may still be running · refresh (⌘R) until it is over, \
             then discard them all",
        ),
        Some(edits::Unjudged::Running) => Some(
            "the commit that sent these may still be running · refresh (⌘R) until it is over, \
             then say whether it landed",
        ),
        Some(edits::Unjudged::Unread) => Some(
            "this page was not read after the commit · refresh (⌘R), then say whether it landed",
        ),
        None if reshaped && !landed => Some(HELD_RESHAPED),
        None => None,
    }
}

/// Why nothing is staged or committed, if nothing is, in the words that
/// name the way out of that state. A set held on a table whose columns
/// changed has its own: the discard the reshaped hint names is one a held
/// set refuses, and its way out is the popover.
fn stale_reason(reshaped: bool, unrefreshed: bool, held: bool) -> Option<&'static str> {
    match (reshaped, unrefreshed, held) {
        (true, _, true) => Some(HELD_RESHAPED),
        (true, _, false) => Some(RESHAPED),
        (false, true, _) => Some(UNREFRESHED),
        (false, false, true) => Some(IN_DOUBT),
        (false, false, false) => None,
    }
}

/// What became of the page asked for after a commit.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Page<'a> {
    /// It arrived, and is on screen.
    Read,
    /// It arrived with other columns than the grid's, and was dropped
    /// because edits are staged against these.
    Reshaped,
    /// It did not arrive. The reason.
    Failed(&'a str),
}

/// Open the transaction on session `sid` and run a staged set's statements
/// in it, each checked for the one row it must touch. The first that fails
/// stops the run with the reason, and nothing has landed: the caller's
/// release of the session rolls the transaction back.
fn run_statements(conn: &Conn, sid: &str, stmts: &[edits::Statement]) -> Result<(), String> {
    harbor_client::exec(conn, "BEGIN", None, Some(sid))?;
    for stmt in stmts {
        let r = harbor_client::exec(conn, &stmt.sql, Some(stmt.params.clone()), Some(sid))?;
        match stmt.expectation {
            // The engine answers UPDATE/DELETE with one count row; anything
            // but exactly 1 means the row is not what was fetched.
            edits::StatementExpectation::AffectedOne => {
                let affected = crate::sql::count_of(&r).unwrap_or(0);
                if affected != 1 {
                    return Err(format!(
                        "a row changed since you read it ({affected} rows matched) — refresh and retry"
                    ));
                }
            }
            // Only a duplicate selects its row, from the row it copies: none
            // back means that row is gone, and no refresh brings it back,
            // since the draft names that row until it is discarded.
            edits::StatementExpectation::ReturnedOne if r.rows.is_empty() => {
                return Err("a duplicated row's source is gone — discard that duplicate \
                            (⌘Z, or the review popover)"
                    .to_string());
            }
            edits::StatementExpectation::ReturnedOne if r.rows.len() != 1 => {
                return Err(format!("insert returned {} rows instead of 1", r.rows.len()));
            }
            edits::StatementExpectation::ReturnedOne => {}
        }
    }
    Ok(())
}

/// Read the answer to the COMMIT request. An error Harbor reports is a
/// verdict: the engine refused the commit, or the session was gone, and
/// either way the transaction is rolled back. A request that could not be
/// sent did nothing, and releasing the session rolls its transaction back.
/// Anything else — a timeout, a dropped tunnel, an answer cut short — came
/// after the request was sent, and says nothing about whether the server
/// committed first.
fn commit_verdict(answer: Result<harbor_client::QueryResult, harbor_client::Failure>) -> Committed {
    use harbor_client::Failure;
    match answer {
        Ok(_) => Committed::Landed,
        Err(refused @ Failure::Refused { .. }) => Committed::Refused(refused.to_string()),
        Err(Failure::Unsent(message)) => Committed::Refused(message),
        Err(Failure::Unanswered(message)) => Committed::InDoubt(message),
    }
}

/// The status line once the page fetch after a commit has settled. None
/// when the commit landed and the page shows it.
fn commit_status(after: &PostCommit, page: Page) -> Option<String> {
    match (after, page) {
        // A commit that landed left nothing staged, so its page is adopted
        // whatever its columns.
        (PostCommit::Landed, Page::Read | Page::Reshaped) => None,
        (PostCommit::Landed, Page::Failed(why)) => Some(format!(
            "committed, but this page could not be read again ({why}) · its edited cells show \
             what was typed, not what the database stored · refresh (⌘R) before editing"
        )),
        (PostCommit::InDoubt { message, unsettled: None }, Page::Read) => Some(format!(
            "COMMIT got no answer ({message}), so the changes may or may not have landed · the \
             commit is over, and this page was read after it · {HELD}"
        )),
        (PostCommit::InDoubt { message, unsettled: Some(_) }, Page::Read) => Some(format!(
            "COMMIT got no answer ({message}) and may still be running, so this page may not \
             show its outcome yet · the staged changes are held, off the page · refresh (⌘R) \
             until the commit is over"
        )),
        (PostCommit::InDoubt { message, unsettled: None }, Page::Reshaped) => Some(format!(
            "COMMIT got no answer ({message}), so the changes may or may not have landed · \
             {HELD_RESHAPED}"
        )),
        (PostCommit::InDoubt { message, unsettled: Some(_) }, Page::Reshaped) => Some(format!(
            "COMMIT got no answer ({message}) and may still be running · this table’s columns \
             changed in the database, so the staged changes are held and cannot be staged again \
             · refresh (⌘R) until the commit is over, then click the count and discard them all"
        )),
        (PostCommit::InDoubt { message, .. }, Page::Failed(why)) => Some(format!(
            "COMMIT got no answer ({message}), so the changes may or may not have landed, and \
             this page could not be read again ({why}) · the staged changes are held, off the \
             page · refresh (⌘R)"
        )),
    }
}

/// What to do with a set held after a commit that got no answer, once
/// there is a page to judge it against.
const HELD: &str = "the staged changes are held, off the page: click the count and say whether \
                    the commit landed";

/// A gesture that would stage into, discard from, undo or commit a held set.
const IN_DOUBT: &str = "the last commit got no answer and may have landed, so the staged changes \
                        are held · click the count and say whether it landed: all of them are \
                        dropped, or all of them staged again";

/// A set held on a table whose columns changed since it was staged. It
/// cannot be staged again against them, landed or not, so one way out is
/// left, and this names it: the popover's discard, which a held set takes
/// where it refuses the keyboard's.
const HELD_RESHAPED: &str = "this table’s columns changed in the database, and the staged changes \
                             are held after a commit that got no answer · they cannot be staged \
                             against the changed columns: click the count and discard them all \
                             to load the table as it is";

/// The status line while the page on screen predates a commit that landed.
const UNREFRESHED: &str = "the last commit landed, but this page was not read again: its edited \
                           cells show what was typed, not what the database stored, and the rows \
                           it deleted are still drawn · refresh (⌘R) before editing";

/// The status line while edits are staged against columns the table does
/// not have.
const RESHAPED: &str = "this table’s columns changed in the database · review the staged edits, \
                        then discard them (⌘⇧⌫) to load the table as it is";

/// The status line of a grid handed a stash its table has outgrown.
fn orphaned(staged: usize) -> String {
    format!(
        "this table’s columns changed in the database · {staged} staged {} dropped",
        if staged == 1 { "edit was" } else { "edits were" }
    )
}

/// Whether a fetched page has the columns a grid was built with: the same
/// names and the same DuckDB types, in the same order. The types decide how
/// every staged value is bound (`edits::placeholder_for`) and the names
/// decide where, so a page that differs in either is another table's.
fn same_columns(have: &[wire::Column], page: &[wire::Column]) -> bool {
    have.len() == page.len()
        && have
            .iter()
            .zip(page)
            .all(|(a, b)| a.name == b.name && a.duckdb_type == b.duckdb_type)
}

/// The row ⌘D would copy.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Lead {
    /// A fetched row, with or without staged updates.
    Persisted,
    /// A staged INSERT.
    Draft,
    /// A row staged for DELETE.
    Deleted,
}

/// Why ⌘D copies nothing, when it copies nothing. A duplicate's INSERT
/// reads its cells from the source row in the database, and a draft has no
/// row there to read.
fn duplicate_refusal(lead: Option<Lead>) -> Option<&'static str> {
    match lead {
        Some(Lead::Persisted) => None,
        None => Some("select a row to duplicate"),
        Some(Lead::Draft) => Some(
            "a new row is not in the database yet, so there is nothing to copy it from — \
             commit it (⌘S), then duplicate it",
        ),
        Some(Lead::Deleted) => {
            Some("this row is staged for deletion — discard the delete to duplicate it")
        }
    }
}

/// Wire values -> render-ready cell text (None = NULL), once per page.
/// Render never performs conversion or allocation.
fn display_rows(rows: &[Vec<Value>]) -> Vec<Vec<Option<SharedString>>> {
    rows.iter()
        .map(|row| {
            row.iter()
                .map(display_value)
                .collect()
        })
        .collect()
}

fn display_value(value: &Value) -> Option<SharedString> {
    match value {
        Value::Null => None,
        Value::String(s) => Some(SharedString::from(s.clone())),
        other => Some(SharedString::from(other.to_string())),
    }
}

/// Build one duplicate-row INSERT's cells from the source row's fetched
/// text. A cell reads its value from the source row in SQL, so the copy is
/// the engine's own; a cell with a staged update carries that update's
/// text and bound value, which the database does not hold yet. Hidden
/// rowid, declared primary keys, and generated expressions stay absent so
/// DuckDB supplies the new row's identity and derived values.
fn duplicate_cells(
    fetched: Vec<Option<SharedString>>,
    staged: &std::collections::BTreeMap<usize, edits::CellEdit>,
    first_schema: usize,
    pk_ix: &[usize],
    generated: &[bool],
) -> Vec<(usize, Option<SharedString>, edits::Bind)> {
    fetched
        .into_iter()
        .enumerate()
        .skip(first_schema)
        .filter(|(col, _)| {
            !pk_ix.contains(col) && !generated.get(*col).copied().unwrap_or(false)
        })
        .map(|(col, text)| match staged.get(&col) {
            Some(cell) => (col, cell.text.clone(), cell.bind.clone()),
            None => (col, text, edits::Bind::Source),
        })
        .collect()
}

/// How wide a tag is: its text in the UI font at the tag size (an
/// estimate, a little generous) plus the pill's 5px padding a side.
fn tag_width(tag: &str, zoom: f32) -> Pixels {
    px(tag.chars().count() as f32 * TAG_TEXT * 0.7 * zoom + 10.)
}

/// Fills the open cell editor with a value and confirms it.
type PickValue = Rc<dyn Fn(SharedString, &mut Window, &mut App)>;

/// What an untouched cell of a draft row becomes when the row commits —
/// the cell is left out of the INSERT, so the database decides.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum DraftHint {
    /// The column computes itself from this expression.
    Generated(SharedString),
    /// The column's default expression fills it.
    Default(SharedString),
    /// NOT NULL with no default: the row cannot commit without a value.
    Required,
    /// Left blank, it stores NULL.
    Null,
}

impl DraftHint {
    /// What the cell becomes, for the inspector.
    fn describe(&self) -> SharedString {
        match self {
            DraftHint::Generated(expr) if !expr.is_empty() => format!("generated: {expr}").into(),
            DraftHint::Generated(_) => "generated".into(),
            DraftHint::Default(expr) => format!("default: {expr}").into(),
            DraftHint::Required => "required: NOT NULL, no default".into(),
            DraftHint::Null => "NULL".into(),
        }
    }
}

fn build_columns(
    names: &[SharedString],
    visible: &[usize],
    with_gutter: bool,
) -> Vec<TableColumn> {
    // Column 0 is the row-number gutter; its render_td owns every edge.
    let gutter = with_gutter.then(|| {
        TableColumn::new("#", "#")
            .width(px(gutter_width(1)))
            .paddings(Edges::all(px(0.)))
            .resizable(false)
            .movable(false)
            .selectable(false)
            // Sheets' row headers never leave: the gutter pins to the
            // left while the data columns scroll beneath it.
            .fixed_left()
    });
    gutter
        .into_iter()
        .chain(visible.iter().map(|&i| {
            // Left padding stays on the table's cell wrapper; the other
            // edges go to zero so render_td can reach them (its divider
            // and text inset live there). The width is a placeholder:
            // fit_widths sizes every column from its content before the
            // first paint.
            TableColumn::new(format!("c{i}"), names[i].clone())
                .width(px(100.))
                // A drag narrows a column to 10px and widens it to
                // 1200px, the widest a column grows by hand; a fit to
                // content may set it wider.
                .min_width(px(10.))
                .max_width(px(1200.))
                .paddings(Edges {
                    left: px(PANE_INSET),
                    right: px(0.),
                    top: px(0.),
                    bottom: px(0.),
                })
        }))
        .collect()
}

/// Whether a column right-aligns as numbers do: judged by the type's own
/// name, so an INTERVAL, an `INTEGER[]` and an `ENUM('POINT')` do not.
fn numeric(ty: &str) -> bool {
    edits::is_numeric_type(ty)
}

fn wrapped_step(len: usize, position: usize, delta: i32) -> usize {
    (position as i32 + delta).rem_euclid(len as i32) as usize
}

/// The delegate mirrors TableState selection except for a staged row: its
/// missing library selection is intentional presentation, not a user Escape.
fn should_reconcile_selection(
    table_selection: Option<usize>,
    mirror: Option<usize>,
    mirror_is_dirty: bool,
) -> bool {
    mirror != table_selection
        && !(table_selection.is_none() && mirror.is_some() && mirror_is_dirty)
}

#[cfg(test)]
mod tests {
    use super::{
        ClickKind, DraftHint, Lead, RowSelection, duplicate_cells, tag_width, duplicate_refusal, orphaned, same_columns, should_reconcile_selection,
        wrapped_step, Committed, Page, PostCommit, commit_status, commit_verdict, settle_fetch,
        stale_reason, verdict_refusal,
    };
    use crate::edits::{self, Bind, CellEdit, Edits};
    use gpui_kit::{Modifiers, SharedString};
    use serde_json::json;
    use std::collections::BTreeMap;

    #[test]
    fn required_tag_width_scales_with_zoom() {
        assert_eq!(f32::from(tag_width("REQUIRED", 1.)), 66.);
        assert_eq!(f32::from(tag_width("REQUIRED", 1.5)), 94.);
    }

    #[test]
    fn draft_hints_describe_what_the_cell_becomes() {
        let sequence = DraftHint::Default("nextval('id')".into());
        assert_eq!(sequence.describe().as_ref(), "default: nextval('id')");
        assert_eq!(DraftHint::Required.describe().as_ref(), "required: NOT NULL, no default");
        assert_eq!(DraftHint::Null.describe().as_ref(), "NULL");
        let generated = DraftHint::Generated("a + b".into());
        assert_eq!(generated.describe().as_ref(), "generated: a + b");
        assert_eq!(DraftHint::Generated("".into()).describe().as_ref(), "generated");
    }

    #[test]
    fn insert_metadata_ranks_generated_then_default_then_required() {
        let col = |name: &str, notnull, dflt: Option<&str>, generated| crate::structure::StructCol {
            name: name.into(),
            ty: "INTEGER".into(),
            notnull,
            dflt: dflt.map(Into::into),
            generated,
            generation_expression: generated.then(|| "a + 1".into()),
            pk: false,
        };
        let structure = crate::structure::TableStructure {
            cols: vec![
                col("g", true, None, true),
                col("d", true, Some("nextval('id')"), false),
                col("r", true, None, false),
                col("n", false, None, false),
            ],
            ddl: None,
        };
        let names: Vec<SharedString> = ["g", "d", "r", "n", "rowid"].map(Into::into).into();
        let (_, _, _, hints, types) = super::insert_metadata(&names, Some(&structure));
        assert_eq!(types[4], None);
        assert_eq!(
            hints,
            vec![
                DraftHint::Generated("a + 1".into()),
                DraftHint::Default("nextval('id')".into()),
                DraftHint::Required,
                DraftHint::Null,
                DraftHint::Null,
            ]
        );
    }

    #[test]
    fn tab_steps_wrap_at_both_ends_of_a_row() {
        assert_eq!(wrapped_step(4, 0, 1), 1);
        assert_eq!(wrapped_step(4, 3, 1), 0);
        assert_eq!(wrapped_step(4, 3, -1), 2);
        assert_eq!(wrapped_step(4, 0, -1), 3);
    }

    #[test]
    fn an_unpainted_dirty_row_is_not_mistaken_for_escape() {
        assert!(!should_reconcile_selection(None, Some(0), true));
        assert!(should_reconcile_selection(None, Some(0), false));
        assert!(should_reconcile_selection(Some(1), Some(0), true));
        assert!(!should_reconcile_selection(Some(0), Some(0), false));
    }

    fn rows(sel: &RowSelection) -> Vec<usize> {
        sel.rows.iter().copied().collect()
    }

    #[test]
    fn a_plain_click_replaces_the_selection_and_moves_the_anchor() {
        let mut sel = RowSelection::default();
        assert_eq!(sel.click(4, ClickKind::Plain), Some(4));
        sel.click(7, ClickKind::Toggle);
        assert_eq!(sel.click(2, ClickKind::Plain), Some(2));
        assert_eq!(rows(&sel), vec![2]);
        assert_eq!(sel.anchor, Some(2));
    }

    #[test]
    fn a_command_click_toggles_one_row_and_keeps_the_lead_unless_it_left() {
        let mut sel = RowSelection::default();
        sel.click(3, ClickKind::Plain);
        assert_eq!(sel.click(6, ClickKind::Toggle), Some(6));
        assert_eq!(sel.click(1, ClickKind::Toggle), Some(1));
        assert_eq!(rows(&sel), vec![1, 3, 6]);
        // Removing a row that is not the lead leaves the lead alone.
        assert_eq!(sel.click(3, ClickKind::Toggle), Some(1));
        // Removing the lead hands it to the last remaining row.
        assert_eq!(sel.click(1, ClickKind::Toggle), Some(6));
        assert_eq!(sel.anchor, Some(1));
        // Emptying the selection leaves no lead and no anchor to span from.
        assert_eq!(sel.click(6, ClickKind::Toggle), None);
        assert!(rows(&sel).is_empty());
        assert_eq!(sel.anchor, Some(6));
    }

    #[test]
    fn a_shift_click_spans_from_the_anchor_in_either_direction() {
        let mut sel = RowSelection::default();
        sel.click(5, ClickKind::Plain);
        assert_eq!(sel.click(8, ClickKind::Extend), Some(8));
        assert_eq!(rows(&sel), vec![5, 6, 7, 8]);
        // A second ⇧-click re-spans from the same anchor, replacing the first.
        assert_eq!(sel.click(3, ClickKind::Extend), Some(3));
        assert_eq!(rows(&sel), vec![3, 4, 5]);
        assert_eq!(sel.anchor, Some(5));
        // With nothing to span from, ⇧-click is a plain click.
        let mut fresh = RowSelection::default();
        assert_eq!(fresh.click(2, ClickKind::Extend), Some(2));
        assert_eq!(rows(&fresh), vec![2]);
    }

    #[test]
    fn a_lead_chosen_elsewhere_joins_or_replaces_the_selection() {
        let mut sel = RowSelection::default();
        sel.click(1, ClickKind::Plain);
        sel.click(4, ClickKind::Extend);
        sel.lead_on(Some(2));
        assert_eq!(rows(&sel), vec![1, 2, 3, 4]);
        assert_eq!(sel.lead, Some(2));
        sel.lead_on(Some(9));
        assert_eq!(rows(&sel), vec![9]);
        sel.lead_on(None);
        assert_eq!(sel, RowSelection::default());
    }

    #[test]
    fn shift_outranks_command_and_a_bare_click_is_plain() {
        let cmd = Modifiers { platform: true, ..Modifiers::default() };
        let both = Modifiers { platform: true, shift: true, ..Modifiers::default() };
        assert_eq!(ClickKind::of(&Modifiers::default()), ClickKind::Plain);
        assert_eq!(ClickKind::of(&cmd), ClickKind::Toggle);
        assert_eq!(ClickKind::of(&both), ClickKind::Extend);
    }

    #[test]
    fn duplicate_reads_from_the_source_and_omits_identity_and_generated_columns() {
        let txt = |s: &str| Some(SharedString::from(s.to_string()));
        let cells = duplicate_cells(
            vec![txt("99"), txt("7"), txt("00123"), None, txt("14")],
            &BTreeMap::new(),
            0,
            &[0],
            &[false, false, false, false, true],
        );
        assert_eq!(
            cells,
            vec![
                (1, txt("7"), Bind::Source),
                (2, txt("00123"), Bind::Source),
                (3, None, Bind::Source),
            ]
        );

        // A staged update is not in the database yet: its cell is bound,
        // showing the staged text. A staged key cell is still omitted.
        let staged = BTreeMap::from([
            (0, CellEdit { original: txt("99"), text: txt("100"), bind: Bind::Value(json!(100)), copied: None }),
            (2, CellEdit { original: txt("00123"), text: None, bind: Bind::Value(json!(null)), copied: None }),
        ]);
        let cells = duplicate_cells(
            vec![txt("99"), txt("7"), txt("00123")],
            &staged,
            0,
            &[0],
            &[false, false, false],
        );
        assert_eq!(
            cells,
            vec![(1, txt("7"), Bind::Source), (2, None, Bind::Value(json!(null)))]
        );

        // A keyless table's hidden rowid occupies schema column zero.
        let cells = duplicate_cells(
            vec![txt("99"), txt("7"), txt("Ada")],
            &BTreeMap::new(),
            1,
            &[0],
            &[false, false, false],
        );
        assert_eq!(cells.iter().map(|(col, _, _)| *col).collect::<Vec<_>>(), vec![1, 2]);
    }

    fn col(name: &str, ty: &str) -> wire::Column {
        wire::Column { name: Some(name.to_string()), duckdb_type: ty.to_string(), ..Default::default() }
    }

    #[test]
    fn a_page_with_other_names_or_types_is_another_tables() {
        let have = [col("id", "INTEGER"), col("name", "VARCHAR"), col("b", "BLOB")];
        assert!(same_columns(&have, &have.clone()));
        assert!(same_columns(&[], &[]));
        // ALTER … TYPE: the placeholder a value binds through changes.
        assert!(!same_columns(&have, &[col("id", "INTEGER"), col("name", "VARCHAR"), col("b", "VARCHAR")]));
        assert!(!same_columns(&have, &[col("id", "BIGINT"), col("name", "VARCHAR"), col("b", "BLOB")]));
        // A parameter is part of the type.
        assert!(!same_columns(&[col("d", "DECIMAL(10,2)")], &[col("d", "DECIMAL(12,2)")]));
        // ADD, DROP and RENAME COLUMN, and columns that traded places.
        assert!(!same_columns(&have, &have[..2]));
        assert!(!same_columns(&have[..2], &have));
        assert!(!same_columns(&have, &[col("id", "INTEGER"), col("title", "VARCHAR"), col("b", "BLOB")]));
        assert!(!same_columns(&have, &[col("id", "INTEGER"), col("b", "BLOB"), col("name", "VARCHAR")]));
        // What a type implies on the wire is not compared apart from it.
        let mut lossy = col("id", "INTEGER");
        lossy.lossless = !lossy.lossless;
        assert!(same_columns(&have[..1], &[lossy]));
    }

    #[test]
    fn command_d_says_why_it_copied_nothing() {
        assert_eq!(duplicate_refusal(Some(Lead::Persisted)), None);
        let draft = duplicate_refusal(Some(Lead::Draft)).expect("a reason");
        assert!(draft.contains("not in the database yet") && draft.contains("\u{2318}S"), "{draft}");
        let deleted = duplicate_refusal(Some(Lead::Deleted)).expect("a reason");
        assert!(deleted.contains("staged for deletion"), "{deleted}");
        assert_eq!(duplicate_refusal(None), Some("select a row to duplicate"));
    }

    #[test]
    fn a_commit_whose_answer_is_lost_is_in_doubt_not_failed() {
        use harbor_client::Failure;
        let ok = harbor_client::QueryResult { columns: vec![], rows: vec![], row_count: 0, time_ms: 0 };
        assert_eq!(commit_verdict(Ok(ok)), Committed::Landed);
        // Harbor's own error is a verdict: the engine refused the commit, or
        // the session was reclaimed. Nothing landed.
        let refused = Failure::Refused {
            code: "sql_error".into(),
            message: "TransactionContext Error: Failed to commit".into(),
        };
        assert_eq!(
            commit_verdict(Err(refused)),
            Committed::Refused("sql_error: TransactionContext Error: Failed to commit".into())
        );
        let gone = Failure::Refused { code: "no_such_session".into(), message: "no such session".into() };
        assert!(matches!(commit_verdict(Err(gone)), Committed::Refused(_)));
        // A COMMIT that could not be sent did nothing: there is no doubt.
        let unsent = "query: Connection refused (os error 61)";
        assert_eq!(commit_verdict(Err(Failure::Unsent(unsent.into()))), Committed::Refused(unsent.into()));
        // A timeout or a dropped tunnel after it was sent is no verdict at all.
        for lost in ["query: Resource temporarily unavailable (os error 35)", "stream: connection closed mid-chunk", "HTTP 502"] {
            assert_eq!(commit_verdict(Err(Failure::Unanswered(lost.into()))), Committed::InDoubt(lost.into()));
        }
    }

    #[test]
    fn the_status_after_a_commit_says_what_is_known() {
        assert_eq!(commit_status(&PostCommit::Landed, Page::Read), None);
        assert_eq!(commit_status(&PostCommit::Landed, Page::Reshaped), None);
        let stale = commit_status(&PostCommit::Landed, Page::Failed("query: refused")).unwrap();
        assert!(stale.starts_with("committed, but") && stale.contains("query: refused") && stale.contains("⌘R"));
        assert!(stale.contains("what was typed, not what the database stored"));
        assert!(!stale.contains("held"), "the staged set was cleared: {stale}");

        let doubt = |running: bool| PostCommit::InDoubt {
            message: "query: timed out".into(),
            unsettled: running.then(|| "session-1".to_string()),
        };
        // The commit's session was seen to end before the page was read: the
        // page is its outcome, and says so.
        let settled = commit_status(&doubt(false), Page::Read).unwrap();
        assert!(settled.contains("no answer (query: timed out)") && settled.contains("may or may not have landed"));
        assert!(settled.contains("the commit is over, and this page was read after it"));
        assert!(settled.ends_with("say whether the commit landed"));
        // It was not: the page is not called the outcome, and no verdict is
        // invited yet.
        let running = commit_status(&doubt(true), Page::Read).unwrap();
        assert!(running.contains("may still be running") && running.contains("may not show its outcome yet"));
        assert!(!running.contains("read after it") && !running.contains("say whether"));
        assert!(running.ends_with("refresh (⌘R) until the commit is over"));
        let unread = commit_status(&doubt(false), Page::Failed("HTTP 503")).unwrap();
        assert!(unread.contains("could not be read again (HTTP 503)") && !unread.contains("read after it"));
        // Every one of them says the set is held.
        for status in [settled, running, unread] {
            assert!(status.contains("the staged changes are held, off the page"), "{status}");
        }
        // The page came back with other columns: it is not called unread,
        // and the way out named is one that works on a held set.
        for running in [false, true] {
            let reshaped = commit_status(&doubt(running), Page::Reshaped).unwrap();
            assert!(reshaped.contains("columns changed in the database"), "{reshaped}");
            assert!(reshaped.contains("click the count and discard them all"), "{reshaped}");
            assert!(!reshaped.contains("could not be read") && !reshaped.contains("⌘⇧⌫"), "{reshaped}");
            assert_eq!(reshaped.contains("refresh (⌘R) until the commit is over"), running);
        }
    }

    #[test]
    fn a_held_set_on_a_reshaped_table_can_be_judged_and_is_told_how() {
        use edits::Unjudged::{Running, Unread};
        // An unanswered COMMIT, then the table is altered: the fetch after
        // the commit reads a page with other columns and drops it. The
        // database was read all the same, so the set is not left Unread.
        let doubt = |unsettled: Option<&str>| PostCommit::InDoubt {
            message: "query: timed out".into(),
            unsettled: unsettled.map(str::to_string),
        };
        let mut e = staged();
        settle_fetch(Some(&doubt(None)), true, Some(&mut e));
        assert_eq!(e.unjudged(), None);
        // It cannot be staged again against the changed columns, and the
        // refusal names the way out. It can be dropped, and is.
        let refusal = verdict_refusal(false, true, e.unjudged()).unwrap();
        assert!(refusal.contains("click the count and discard them all"), "{refusal}");
        assert_eq!(verdict_refusal(true, true, e.unjudged()), None);
        assert!(e.judge(true) && e.is_empty());

        // The commit was still running when that page was read, and is over
        // by a later refresh, whose page is dropped as reshaped again: each
        // page query that answers is told to the set, kept or not.
        let mut e = staged();
        settle_fetch(Some(&doubt(Some("session-1"))), true, Some(&mut e));
        assert_eq!(e.unjudged(), Some(Running));
        e.fetched(Some(false));
        assert_eq!(e.unjudged(), Some(Running), "still running at that refresh");
        e.fetched(Some(true));
        assert_eq!(e.unjudged(), None);
        assert_eq!(verdict_refusal(true, true, e.unjudged()), None);
        // The page after the commit failed outright, and a later one is read
        // and dropped as reshaped.
        let mut e = staged();
        settle_fetch(Some(&doubt(None)), false, Some(&mut e));
        assert_eq!(e.unjudged(), Some(Unread));
        e.fetched(None);
        assert_eq!(e.unjudged(), None);

        // The hint in each state names an action that works there. A held
        // set refuses the keyboard discard the reshaped hint names, so held
        // and reshaped it is sent to the popover.
        let hint = stale_reason(true, false, true).unwrap();
        assert!(hint.contains("click the count and discard them all") && !hint.contains("⌘⇧⌫"), "{hint}");
        assert_eq!(stale_reason(true, true, true), Some(hint));
        assert!(stale_reason(true, false, false).unwrap().contains("⌘⇧⌫"));
        assert!(stale_reason(false, true, false).unwrap().contains("refresh (⌘R)"));
        assert!(stale_reason(false, false, true).unwrap().contains("click the count and say whether it landed"));
        assert_eq!(stale_reason(false, false, false), None);
    }

    fn staged() -> Edits {
        let mut e = Edits::new(
            "\"main\".\"t\"".into(),
            vec!["id".into()],
            vec!["id".into(), "name".into()],
            vec!["INTEGER".into(), "VARCHAR".into()],
        );
        e.stage_delete(vec![json!(7)]);
        e.stage_cell(vec![json!(3)], 0, Some("3".into()), Some("7".into()), json!(7));
        e
    }

    #[test]
    fn the_fetch_after_an_unanswered_commit_holds_the_set_off_the_page() {
        // What the fetch's completion does, in its order: settle what the
        // commit left, then draw the projection.
        let doubt = |unsettled: Option<&str>| PostCommit::InDoubt {
            message: "query: timed out".into(),
            unsettled: unsettled.map(str::to_string),
        };
        // The page arrived, read after the commit was over: the set is held,
        // nothing of it is drawn, and it can be judged.
        let mut e = staged();
        assert_eq!(e.projection().len(), 2);
        assert!(!settle_fetch(Some(&doubt(None)), true, Some(&mut e)));
        assert!(e.in_doubt() && e.projection().is_empty());
        assert_eq!((e.len(), e.unjudged()), (2, None));
        // The commit may still be running: held, and not yet to be judged.
        let mut e = staged();
        settle_fetch(Some(&doubt(Some("session-1"))), true, Some(&mut e));
        assert!(e.in_doubt() && e.projection().is_empty());
        assert_eq!((e.unsettled(), e.unjudged()), (Some("session-1"), Some(edits::Unjudged::Running)));
        // The page query got no answer: held, and nothing to judge against.
        let mut e = staged();
        assert!(!settle_fetch(Some(&doubt(None)), false, Some(&mut e)), "nothing landed for certain");
        assert_eq!(e.unjudged(), Some(edits::Unjudged::Unread));

        // A commit that landed and a page that did not arrive: the page on
        // screen predates the commit. With the page, nothing is left over.
        assert!(settle_fetch(Some(&PostCommit::Landed), false, None));
        assert!(!settle_fetch(Some(&PostCommit::Landed), true, None));
        // An ordinary fetch settles nothing and holds nothing.
        let mut e = staged();
        assert!(!settle_fetch(None, true, Some(&mut e)));
        assert!(!e.in_doubt() && e.projection().len() == 2);
    }

    #[test]
    fn a_verdict_on_a_held_set_waits_for_a_page_read_after_the_commit() {
        use edits::Unjudged::{Running, Unread};
        // The commit may still be running: staged again now, a second
        // transaction could insert what the first is still inserting; and
        // dropped as landed, the set would be lost if it then rolls back.
        for landed in [true, false] {
            assert!(verdict_refusal(landed, false, Some(Running)).unwrap().contains("may still be running"));
            assert!(verdict_refusal(landed, false, Some(Unread)).unwrap().contains("was not read after the commit"));
            let reshaped = verdict_refusal(landed, true, Some(Running)).unwrap();
            assert!(reshaped.contains("may still be running") && reshaped.contains("discard them all"));
            assert!(!reshaped.contains("say whether it landed"), "the reshaped popover offers no such choice");
        }
        // Judgeable, both verdicts are taken.
        assert_eq!(verdict_refusal(true, false, None), None);
        assert_eq!(verdict_refusal(false, false, None), None);
        // Against columns the table does not have, the set is not staged
        // again; dropping it is the way out.
        assert_eq!(verdict_refusal(false, true, None), Some(super::HELD_RESHAPED));
        assert_eq!(verdict_refusal(true, true, None), None);

        // Staged again after the verdict, the set is drawn and sent whole;
        // judged landed, nothing is left to draw or send.
        let mut e = staged();
        settle_fetch(Some(&PostCommit::InDoubt { message: "x".into(), unsettled: None }), true, Some(&mut e));
        assert!(e.judge(false));
        assert_eq!((e.projection().len(), e.statements().len()), (2, 2));
        settle_fetch(Some(&PostCommit::InDoubt { message: "x".into(), unsettled: None }), true, Some(&mut e));
        assert!(e.judge(true));
        assert!(e.projection().is_empty() && e.statements().is_empty());
    }

    /// Run `e` as ⌘S does, in one session's transaction, and commit it.
    fn commit_live(conn: &harbor_client::Conn, e: &Edits) -> Result<(), String> {
        let sid = harbor_client::session_new(conn)?;
        let run = super::run_statements(conn, &sid, &e.statements())
            .and_then(|()| harbor_client::exec(conn, "COMMIT", None, Some(&sid)).map(drop));
        harbor_client::session_release(conn, &sid);
        run
    }

    /// Staged sets that move keys commit, and a full-precision DOUBLE key
    /// names its row, which also needs a Harbor that reads the param as
    /// exactly as DuckTable writes it. Live: needs `HARBOR_LIVE_DB`, a
    /// scratch database file, as the probes in harbor-client's
    /// `tests/live.rs` do.
    #[test]
    #[ignore]
    fn staged_sets_that_move_keys_commit() {
        let Some(db) = std::env::var_os("HARBOR_LIVE_DB") else {
            println!("set HARBOR_LIVE_DB to a scratch database file; skipping");
            return;
        };
        let conn = harbor_client::fleet::connect_path(std::path::Path::new(&db)).expect("connect");
        let alone = |sql: &str| harbor_client::exec(&conn, sql, None, None).expect(sql);
        let txt = |s: &str| Some(SharedString::from(s.to_string()));
        alone("DROP TABLE IF EXISTS _dt_order");
        alone("CREATE OR REPLACE SEQUENCE _dt_order_seq START 100");
        alone("CREATE OR REPLACE TABLE _dt_order(id INTEGER PRIMARY KEY DEFAULT nextval('_dt_order_seq'), v VARCHAR)");
        alone("INSERT INTO _dt_order VALUES (1, 'one'), (2, 'two'), (3, 'three'), (7, 'seven'), (9, 'nine')");
        let mut e = Edits::new(
            crate::sql::source("main", "_dt_order"),
            vec!["id".into()],
            vec!["id".into(), "v".into()],
            vec!["INTEGER".into(), "VARCHAR".into()],
        );
        // EDITING.md's example: a DELETE of 7 and a re-key of 3 to 7.
        e.stage_delete(vec![json!(7)]);
        e.stage_cell(vec![json!(3)], 0, txt("3"), txt("7"), json!(7));
        // A chain: 2 to 4, and 1 to the 2 it leaves.
        e.stage_cell(vec![json!(1)], 0, txt("1"), txt("2"), json!(2));
        e.stage_cell(vec![json!(2)], 0, txt("2"), txt("4"), json!(4));
        // A new row keyed as a deleted one.
        e.stage_delete(vec![json!(9)]);
        let draft = e.stage_insert();
        e.stage_insert_cell(&draft, 0, txt("9"), json!(9));
        e.stage_insert_cell(&draft, 1, txt("new"), json!("new"));
        // A duplicate of the row the set re-keys, read as it was.
        e.stage_duplicate(vec![json!(3)], vec![(1, txt("three"), Bind::Source)]);
        commit_live(&conn, &e).expect("the set commits");
        let rows = alone("SELECT id, v FROM _dt_order ORDER BY id").rows;
        assert_eq!(
            rows,
            [(2, "one"), (4, "two"), (7, "three"), (9, "new"), (100, "three")]
                .map(|(id, v)| vec![json!(id), json!(v)])
        );

        alone("CREATE OR REPLACE TABLE _dt_double(k DOUBLE PRIMARY KEY, v INTEGER)");
        alone("INSERT INTO _dt_double VALUES (924.2100000029881, 1)");
        let page = harbor_client::query(&conn, "SELECT k, v FROM _dt_double").expect("page");
        let mut e = Edits::new(
            crate::sql::source("main", "_dt_double"),
            vec!["k".into()],
            vec!["k".into(), "v".into()],
            vec!["DOUBLE".into(), "INTEGER".into()],
        );
        e.stage_cell(vec![page.rows[0][0].clone()], 1, txt("1"), txt("2"), json!(2));
        commit_live(&conn, &e).expect("the key names its row");
        alone("DROP TABLE _dt_order");
        alone("DROP SEQUENCE _dt_order_seq");
        alone("DROP TABLE _dt_double");
    }

    #[test]
    fn an_orphaned_stash_is_counted_in_the_status_line() {
        assert!(orphaned(1).ends_with("1 staged edit was dropped"));
        assert!(orphaned(3).ends_with("3 staged edits were dropped"));
    }
}
