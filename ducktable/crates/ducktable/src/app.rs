//! The root entity: connection state and its lifecycle rules.
//!
//! Every mutation of the phase and its fences lives here. The rendering
//! files (`sidebar.rs`, `content.rs`) read this state and call back into
//! these methods; they never mutate it themselves. The fences are counters
//! (the connect attempt, the connection, the table selection, the
//! refreshes): a late completion compares its own and discards itself.

use crate::sql::Rel;
use crate::util::clone_str;
use gpui_kit::*;
use harbor_client::{fleet, Conn, State};
use wire::open::Kind;

fn catalog_refresh_is_current(
    current_connection: u64,
    current_refresh: u64,
    fenced_connection: u64,
    fenced_refresh: u64,
) -> bool {
    current_connection == fenced_connection && current_refresh == fenced_refresh
}

/// A child surface finished work that may have changed any table. The root
/// owns the catalog snapshot, so grids and query views request a refresh
/// instead of trying to update sidebar counts themselves.
pub(crate) struct CatalogRefreshRequested;

/// A grid's commit has landed or failed: its staged set is now either empty
/// or kept, and a table switch that waited on it can run.
pub(crate) struct CommitSettled;

/// What a database row counts: the tables and views its TABLES tree
/// lists as the database's own.
fn relations(catalog: &harbor_client::Catalog) -> usize {
    catalog.tables.len() + catalog.views.len()
}

/// The row the sidebar lights: the database being dialed while a connect is
/// in flight, the connected one otherwise.
fn active_key<'a>(connecting: Option<&'a DbKey>, connected: Option<&'a DbKey>) -> Option<&'a DbKey> {
    connecting.or(connected)
}

/// The state a row shows. The survey's word stands for a file and for a
/// remote on this machine, whose servers it probes (a miss of the one on
/// screen is checked again, `lost`). It never dials a
/// tunnel, so for the tunneled remote on screen (`tunnel_up` is `Some`)
/// the tunnel answers: running while its SSH process runs and its server
/// is `answering` through it (`watch_tunnel`), stopped once either is not.
fn shown_state(surveyed: State, tunnel_up: Option<bool>, answering: bool) -> State {
    match tunnel_up {
        Some(true) if answering => State::Running,
        Some(_) => State::Stopped,
        None => surveyed,
    }
}

/// How often the tunneled remote on screen is asked `/ready` through its
/// tunnel, how long each read of the answer may take, and how many misses
/// in a row show it as not answering. A slow link that answers, or misses
/// once, stays green; a server gone from its machine shows dim by the
/// second probe, within about thirty seconds.
const PROBE_EVERY: std::time::Duration = std::time::Duration::from_secs(15);
const PROBE_PATIENCE: std::time::Duration = std::time::Duration::from_secs(10);
const MISSES: u32 = 2;

/// Whether the tunneled remote on screen answers, after `unanswered`
/// probes in a row went without an answer.
fn answering(unanswered: u32) -> bool {
    unanswered < MISSES
}

/// Whether the connection on screen has lost its server. For a tunneled
/// remote (`tunnel_up` is `Some`) the SSH process's word stands: a server
/// that does not answer through a running tunnel is shown dim and kept,
/// and comes back green once it answers. For any other the survey gives
/// each server two seconds, which a busy one can miss, so a miss is
/// checked with `/ready` on the connection's own transport, and only a
/// server that fails that too has stopped.
fn lost(surveyed_live: bool, tunnel_up: Option<bool>, ready: impl FnOnce() -> bool) -> bool {
    match tunnel_up {
        Some(up) => !up,
        None => !surveyed_live && !ready(),
    }
}

/// What runs when the quit dialog is cancelled.
#[derive(Debug, PartialEq)]
enum Resume {
    /// The connect the dialog called off when it opened.
    Dial(Aim),
    /// The table switch that waited under it.
    Select(Rel),
    Nothing,
}

/// What the quit dialog held back, in the order it resumes. A connect that
/// was called off comes first and alone: it replaces the grid a waiting
/// table switch was for.
fn after_quit_dialog(called_off: Option<Aim>, deferred: Option<Rel>) -> Resume {
    match (called_off, deferred) {
        (Some(aim), _) => Resume::Dial(aim),
        (None, Some(rel)) => Resume::Select(rel),
        (None, None) => Resume::Nothing,
    }
}

/// What quitting, or leaving a database, would lose or leave unreported.
/// Law 2 (docs/EDITING.md) makes staged changes the only place work lives
/// before ⌘S, so a quit asks before it discards them.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct QuitRisks {
    /// Staged changes not yet sent, over every table that holds some: the
    /// one on screen and those parked for later. The changes of a commit in
    /// flight are not among them: quitting does not simply discard those,
    /// and `committing` says what becomes of them.
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
    /// An export is writing its file (export.rs). Only the app's end cuts
    /// it off: it holds a connection of its own, which leaving a database
    /// does not close.
    pub(crate) exporting: bool,
}

/// The one dialog (docs/EDITING.md, "Dialogs"): its message, its detail,
/// and the label of the button that goes ahead. Cancel is the other button,
/// and the default.
#[derive(Debug, PartialEq)]
pub(crate) struct QuitQuestion {
    pub(crate) message: String,
    pub(crate) detail: String,
    pub(crate) confirm: &'static str,
}

/// What the one dialog asks before: the app ending, by quitting or by the
/// updater's Install and Relaunch, which quits to install and opens the
/// new version; or the connected database left for another, a database's
/// server stopped, or a saved remote removed.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Leaving {
    Quit,
    Relaunch,
    /// Database `from`, the one connected, left for `to`.
    Switch { from: String, to: Aim },
    Stop { name: String, path: std::path::PathBuf },
    Remove { name: String },
}

impl Leaving {
    /// What going ahead is called in the dialog's sentences.
    fn going(&self) -> String {
        match self {
            Leaving::Quit | Leaving::Relaunch => "quitting".to_string(),
            Leaving::Switch { from, .. } => format!("leaving {from}"),
            Leaving::Stop { name, .. } => format!("stopping {name}"),
            Leaving::Remove { name } => format!("removing {name}"),
        }
    }

    /// The question the dialog asks once nothing is left to lose, as when
    /// a commit settles under it.
    pub(crate) fn plain_question(&self) -> QuitQuestion {
        let (message, confirm) = match self {
            Leaving::Quit => ("Quit DuckTable?".to_string(), "Quit"),
            Leaving::Relaunch => ("Install the update now?".to_string(), "Install"),
            Leaving::Switch { from, .. } => (format!("Leave {from}?"), "Leave"),
            Leaving::Stop { name, .. } => (format!("Stop {name}?"), "Stop"),
            Leaving::Remove { name } => (format!("Remove {name}?"), "Remove"),
        };
        QuitQuestion { message, detail: format!("Nothing is left that {} would lose.", self.going()), confirm }
    }
}

/// What restarting the connected database `name` onto a newer harbor ends:
/// its Query transaction, a statement running there, a commit in flight.
/// The staged changes on screen stay, since the connection does.
fn restart_notes(name: &str, risks: &QuitRisks) -> Vec<String> {
    let mut notes = Vec::new();
    if risks.transaction {
        notes.push(format!("The Query view holds a transaction open on {name}, and the restart rolls it back."));
    }
    if risks.running {
        notes.push(format!("A statement is still running on {name}, and the restart cuts it off unreported."));
    }
    if risks.committing {
        notes.push(format!(
            "A commit is still running on {name}: its changes land only if the server has its COMMIT before \
             it stops."
        ));
    }
    notes
}

/// `text` with its first letter in capitals, to begin a sentence.
fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

impl QuitRisks {
    /// The question to ask before quitting, or None when quitting loses
    /// nothing.
    pub(crate) fn question(&self) -> Option<QuitQuestion> {
        self.question_for(&Leaving::Quit)
    }

    /// What is at risk here that was not in `accepted`: what a connect that
    /// began over `accepted` (nothing, or what the dialog asked about and
    /// the user left anyway) would end unasked when it lands.
    fn since(&self, accepted: &QuitRisks) -> QuitRisks {
        QuitRisks {
            staged: self.staged.saturating_sub(accepted.staged),
            tables: self.tables,
            held: self.held.saturating_sub(accepted.held),
            editing: self.editing && !accepted.editing,
            committing: self.committing && !accepted.committing,
            transaction: self.transaction && !accepted.transaction,
            running: self.running && !accepted.running,
            exporting: self.exporting && !accepted.exporting,
        }
    }

    /// The same question, asked of what is about to happen. Installing an
    /// update quits too, so the facts are the same; only the question and
    /// its button name the relaunch, and Cancel's outcome is said, since the
    /// update still waits. A database left behind keeps its staged and held
    /// sets parked for its return (docs/EDITING.md, "Staging"), so a switch
    /// does not count them and a stop says they are kept; removing a remote
    /// forgets them with it.
    pub(crate) fn question_for(&self, leaving: &Leaving) -> Option<QuitQuestion> {
        let going = leaving.going();
        let parked = matches!(leaving, Leaving::Switch { .. });
        let kept = match leaving {
            Leaving::Stop { name, .. } => Some(name.as_str()),
            _ => None,
        };
        let staged = if parked { 0 } else { self.staged };
        let held = if parked { 0 } else { self.held };
        let changes = match staged {
            0 => None,
            1 => Some("1 staged change".to_string()),
            n => Some(format!("{n} staged changes")),
        };
        let mut detail = Vec::new();
        if let Some(changes) = &changes {
            let (verb, them, stay) = if staged == 1 { ("has", "it", "stays") } else { ("have", "them", "stay") };
            let place = if self.tables > 1 { format!(" in {} tables", self.tables) } else { String::new() };
            detail.push(match kept {
                Some(name) => {
                    format!("{changes}{place} {verb} not been committed, and {stay} staged for when {name} is opened again.")
                }
                None => format!("{changes}{place} {verb} not been committed, and {going} discards {them}."),
            });
        }
        if held > 0 {
            let (them, are, they, stay) =
                if held == 1 { ("1 change", "is", "it", "stays") } else { ("changes", "are", "they", "stay") };
            let them = if held == 1 { them.to_string() } else { format!("{held} {them}") };
            detail.push(match kept {
                Some(name) => format!(
                    "{them} {are} held after a commit that got no answer, and {stay} held for when \
                     {name} is opened again."
                ),
                None => format!(
                    "{them} {are} held after a commit that got no answer: {they} may already be in \
                     the database, and {going} drops the held copy."
                ),
            });
        }
        if self.editing {
            detail.push(format!("A cell editor is open: what is typed in it is not staged, and {going} discards it."));
        }
        if self.committing {
            detail.push(match leaving {
                Leaving::Quit | Leaving::Relaunch => "A commit is still running. Quitting ends it unreported: \
                     its changes land only if the server already has its COMMIT, and are rolled back \
                     otherwise."
                    .to_string(),
                Leaving::Remove { .. } => format!(
                    "A commit is still running. {} leaves it unreported: its changes land only if the \
                     server already has its COMMIT, and are not kept here.",
                    sentence(&going)
                ),
                Leaving::Switch { from: name, .. } | Leaving::Stop { name, .. } => format!(
                    "A commit is still running. {} leaves it unreported, and its changes are held for \
                     when {name} is opened again, to say whether they landed.",
                    sentence(&going)
                ),
            });
        }
        if self.transaction {
            detail.push(format!("The Query view holds a transaction open, and {going} rolls it back."));
        }
        if self.running {
            detail.push(format!(
                "A statement is still running in the Query view. {} leaves its outcome unreported.",
                sentence(&going)
            ));
        }
        if self.exporting {
            detail.push(format!("An export is still writing its file. {} stops it, and no file is written.", sentence(&going)));
        }
        if detail.is_empty() {
            return None;
        }
        // Staged changes alone are the common case, and the question names
        // them; anything else is asked plainly, with the facts below it.
        let only_staged =
            !(held > 0 || self.editing || self.committing || self.transaction || self.running || self.exporting);
        let discard =
            |what: &str| changes.as_ref().filter(|_| only_staged).map(|c| format!("Discard {c} and {what}?"));
        let (message, confirm) = match leaving {
            Leaving::Quit => (
                discard("quit").unwrap_or_else(|| "Quit DuckTable?".to_string()),
                if only_staged { "Discard and Quit" } else { "Quit Anyway" },
            ),
            Leaving::Relaunch => {
                detail.push(
                    "Installing the update quits DuckTable. With Cancel it installs when DuckTable \
                     next quits."
                        .to_string(),
                );
                (
                    discard("install the update").unwrap_or_else(|| "Install the update now?".to_string()),
                    if only_staged { "Discard and Install" } else { "Install Anyway" },
                )
            }
            Leaving::Remove { name } => (
                discard(&format!("remove {name}")).unwrap_or_else(|| format!("Remove {name}?")),
                if only_staged { "Discard and Remove" } else { "Remove Anyway" },
            ),
            Leaving::Stop { name, .. } => (format!("Stop {name}?"), "Stop Anyway"),
            Leaving::Switch { from, .. } => (format!("Leave {from}?"), "Leave Anyway"),
        };
        Some(QuitQuestion { message, detail: detail.join(" "), confirm })
    }
}

/// What the window holds for the database on screen, as `risks_of` reads it.
#[derive(Default)]
struct OnScreen<'a> {
    /// The grid's staged and held sets.
    sets: Vec<&'a crate::edits::Edits>,
    committing: bool,
    editing: bool,
    /// The Query view's transaction, and a statement running there.
    transaction: bool,
    running: bool,
    /// An export writing its file, whatever database it reads.
    exporting: bool,
}

/// What going ahead with `leaving` puts at risk. A quit risks everything:
/// what is on screen and every database's parked sets. Leaving one database
/// risks the sets parked for it, and what is on screen only when it is the
/// one connected (`left` is `connected`); a switch with nothing connected
/// risks nothing. The sets of a commit in flight are not counted as staged:
/// `committing` says what becomes of them.
fn risks_of(
    leaving: &Leaving,
    left: Option<&DbKey>,
    connected: Option<&DbKey>,
    screen: OnScreen<'_>,
    parked: &crate::edits::Parked<DbKey>,
) -> QuitRisks {
    let quitting = matches!(leaving, Leaving::Quit | Leaving::Relaunch);
    let parked: Vec<_> = match (quitting, left) {
        (true, _) => parked.sets().collect(),
        (false, Some(db)) => parked.at(db).collect(),
        (false, None) => return QuitRisks::default(),
    };
    let screen = if quitting || left == connected { screen } else { OnScreen::default() };
    let on_screen = if screen.committing { Vec::new() } else { screen.sets };
    let tally = crate::edits::Tally::of(parked.into_iter().chain(on_screen));
    QuitRisks {
        staged: tally.staged,
        tables: tally.tables,
        held: tally.held,
        editing: screen.editing,
        committing: screen.committing,
        transaction: screen.transaction,
        running: screen.running,
        exporting: quitting && screen.exporting,
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
    /// The workspace (docs/OPEN.md): listed first, and called Workspace.
    pub(crate) workspace: bool,
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
    /// File → Open Database URL: the name to save the address under, and
    /// the address. Dialed again, it saves again (`DuckTable::dial`).
    Url { name: String, host: String, port: String },
    /// Files that open in the workspace (docs/OPEN.md), each by its path
    /// and its kind: a data file as a view, a SQLite file attached. Dialed
    /// again, each finds what the first opening made.
    Workspace(Vec<(std::path::PathBuf, Kind)>),
}

impl Aim {
    /// The database file the aim names, when it names one by its path.
    fn file(&self) -> Option<std::path::PathBuf> {
        match self {
            Aim::File(path) => Some(path.clone()),
            Aim::Workspace(_) => harbor_client::paths::workspace().ok(),
            Aim::Row { .. } | Aim::Url { .. } => None,
        }
    }
}

/// What opening each file of an `Aim::Workspace` made or found, or why it
/// did not open, in the order the files came.
pub(crate) type Openings = Vec<Result<harbor_client::workspace::Opened, String>>;

/// Reach the database `aim` names and read what the pane shows of it, and
/// open in it the files an `Aim::Workspace` brings. Blocks; `DuckTable::dial`
/// runs it off the main thread.
fn connect(aim: Aim) -> Result<(Conn, wire::InfoResponse, harbor_client::Catalog, Openings), String> {
    let mut files = Vec::new();
    let conn = match aim {
        Aim::Row { name, path: Some(path) } => fleet::connect_file(&name, &path)?,
        Aim::Row { name, path: None } => fleet::connect_remote(&name)?,
        Aim::File(path) => fleet::connect_path(&path)?,
        Aim::Url { name, host, port } => fleet::connect_remote(&fleet::add_database(&name, &host, &port)?)?,
        Aim::Workspace(opening) => {
            files = opening;
            fleet::connect_path(&harbor_client::workspace::path()?)?
        }
    };
    let info = fleet::info(&conn)?;
    new_enough(&info.harbor_version)?;
    let opened = open_files(&conn, &files);
    let catalog = read_catalog(&conn)?;
    Ok((conn, info, catalog, opened))
}

/// Open each of `files` in the workspace `conn` serves. Blocks.
fn open_files(conn: &Conn, files: &[(std::path::PathBuf, Kind)]) -> Openings {
    files.iter().map(|(file, kind)| harbor_client::workspace::open(conn, file, *kind)).collect()
}

/// The catalog, with what its views need to be read. A view over `ST_Read`
/// does not bind in a server that has not loaded `spatial`, which the
/// engine does not load on demand. A workspace server loads it before it
/// serves; one that has not (a Harbor that does not, or a database other
/// than the workspace) answers such a view with its error, and the
/// extension is loaded here and the catalog read again. Nothing is asked
/// of a server whose views all read. An extension that will not load
/// leaves the view's error to say so. Blocks.
fn read_catalog(conn: &Conn) -> Result<harbor_client::Catalog, String> {
    let catalog = harbor_client::catalog(conn)?;
    let needed = harbor_client::workspace::needed_extensions(&catalog);
    if needed.is_empty() {
        return Ok(catalog);
    }
    let loaded = needed.iter().filter(|e| harbor_client::workspace::load_extension(conn, e).is_ok()).count();
    if loaded == 0 {
        return Ok(catalog);
    }
    harbor_client::catalog(conn)
}

/// The oldest Harbor whose answers DuckTable reads truly (docs/DESIGN.md):
/// from it on, a COMMIT answered `499` kept nothing, and before it one
/// interrupted as it finished could have landed.
const MIN_HARBOR: &str = "0.44.2";

/// Refuse a server older than [`MIN_HARBOR`], saying what it runs. A version
/// that does not read as one is older too: every Harbor since the floor
/// reports its own.
fn new_enough(harbor_version: &str) -> Result<(), String> {
    if !fleet::version_older(harbor_version, MIN_HARBOR) {
        return Ok(());
    }
    let runs = match harbor_version.trim() {
        "" => "does not say which Harbor it runs".to_string(),
        v => format!("runs Harbor {v}"),
    };
    Err(format!(
        "DuckTable needs Harbor {MIN_HARBOR} or later, and this server {runs}. Upgrade Harbor on its \
         machine (harbor update) and restart the server."
    ))
}

pub(crate) enum Phase {
    Idle,
    Connected {
        conn: Conn,
        info: wire::InfoResponse,
        /// The one snapshot everything schema-shaped renders from: tables,
        /// columns, DDL, exact row counts, and the file's size on disk all
        /// arrive in this single document (`/catalog`).
        catalog: Box<harbor_client::Catalog>,
    },
    Failed { name: String, message: String, aim: Aim },
}

pub struct DuckTable {
    pub(crate) rows: Vec<RowVm>,
    pub(crate) phase: Phase,
    /// The connect fence: bumped by every connect and every cancel, so a
    /// connect that lands late discards itself.
    attempt: u64,
    /// Which connection is on screen: bumped at every phase change, so a
    /// catalog refresh of one that has gone discards itself. A connect
    /// called off leaves it as it was.
    connection: u64,
    pub(crate) selected_table: Option<Rel>,
    pub(crate) grid: Option<Entity<crate::grid::Grid>>,
    /// A connect in flight (berth name). The current phase keeps rendering
    /// until the outcome lands — a berth click never blanks the pane.
    pub(crate) connecting: Option<String>,
    /// Which database that connect is aimed at, for the sidebar's highlight:
    /// of two rows that share a name, only the one clicked lights up.
    pub(crate) connecting_key: Option<DbKey>,
    /// What that connect is aimed at, kept so that one the quit dialog
    /// calls off can be dialed again when the dialog is cancelled.
    connecting_aim: Option<Aim>,
    /// The connect the quit dialog called off when it opened.
    called_off: Option<Aim>,
    /// The sidebar's table-name filter; Some = the field is open.
    pub(crate) table_filter: Option<Entity<gpui_kit::component::input::InputState>>,
    /// The sidebar's database-name filter; Some = the field is open.
    pub(crate) berth_filter: Option<Entity<gpui_kit::component::input::InputState>>,
    /// The one dialog is on screen, asking before a quit or before a
    /// database is left. A second ⌘Q, or a click on the close button, while
    /// it is up asks nothing more.
    pub(crate) asking_to_quit: bool,
    /// Fence for table selection: a first-page fetch that finishes after a
    /// newer click discards itself instead of swapping in a stale grid.
    select_seq: u64,
    /// A table chosen while the current grid was committing. Swapping then
    /// would park a staged set whose statements are already running: back
    /// on the table, it would come back staged, and ⌘S would write it twice.
    /// The switch runs when the commit settles (`CommitSettled`).
    deferred_select: Option<Rel>,
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
    /// The database's one Query scratchpad (docs/QUERY.md law 1), made
    /// when it connects, tables or none, and gone with the connection.
    /// Table switches never touch it.
    pub(crate) query: Option<Entity<crate::query::QueryView>>,
    /// Staged edits parked while their table is off-screen (Law 4 in
    /// docs/EDITING.md: staged changes belong to the table, not the
    /// view), per database: a table switch parks the outgoing table's, a
    /// connection that goes parks its grid's. Handed back when the table's
    /// grid is built on that database again.
    staged: crate::edits::Parked<DbKey>,
    /// Each table's header sort, by database and source, for the session:
    /// a grid built for the table again opens under it (`grid::SortChanged`).
    sorts: std::collections::HashMap<(DbKey, String), Vec<crate::sql::SortKey>>,
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
    /// The probes in a row that the tunneled remote on screen left without
    /// an answer (`watch_tunnel`); zero at every phase change.
    unanswered: u32,
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
            connection: 0,
            selected_table: None,
            grid: None,
            connecting: None,
            connecting_key: None,
            connecting_aim: None,
            called_off: None,
            table_filter: None,
            berth_filter: None,
            asking_to_quit: false,
            select_seq: 0,
            deferred_select: None,
            refresh_seq: 0,
            catalog_seq: 0,
            warning: None,
            query: None,
            staged: Default::default(),
            sorts: Default::default(),
            sidebar_resize,
            stopping: std::collections::HashSet::new(),
            leaving: std::collections::HashSet::new(),
            path_copy: None,
            installed_version: None,
            unanswered: 0,
        };
        // Every way out of the app gives its sessions back, the ones that
        // ask nothing included: Quit from the Dock, a logout, the updater's
        // relaunch. The prefs are written too, with the window's frame a
        // move just before the quit left in memory. The hook does it all, so
        // the future it hands back has nothing left to do. The scratchpad
        // lands on disk first, and every SSH tunnel closes last, once the
        // sessions given back through it are: the quit drops nothing that
        // holds a connection, so no tunnel would close itself.
        cx.on_app_quit(|this, cx| {
            if let Some(query) = &this.query {
                query.read(cx).flush_scratch();
            }
            this.release_for_quit(cx);
            crate::export::abandon(cx);
            crate::prefs::save(cx, |_| {});
            fleet::close_tunnels();
            async {}
        })
        .detach();
        // A run that ended without its quit (a crash, a kill) left its
        // tunnels' sockets behind.
        cx.background_executor().spawn(async { fleet::sweep_tunnels() }).detach();
        this.refresh(cx);
        this
    }

    /// What a quit would lose right now (`QuitRisks`).
    pub(crate) fn quit_risks(&self, cx: &App) -> QuitRisks {
        self.risks(&Leaving::Quit, cx)
    }

    /// What going ahead with `leaving` would put at risk: for a quit,
    /// everything the window holds; for a database left behind, the sets
    /// parked for it and, when it is the one connected, what is on screen.
    pub(crate) fn risks(&self, leaving: &Leaving, cx: &App) -> QuitRisks {
        let grid = self.grid.as_ref().map(|g| g.read(cx));
        let query = self.query.as_ref().map(|q| q.read(cx));
        let screen = OnScreen {
            sets: grid.into_iter().flat_map(|g| g.staged_sets()).collect(),
            committing: grid.is_some_and(|g| g.committing),
            editing: grid.is_some_and(|g| g.is_editing()),
            transaction: query.is_some_and(|q| q.in_transaction()),
            running: query.is_some_and(|q| q.is_running()),
            exporting: crate::export::running(cx),
        };
        risks_of(leaving, self.left_by(leaving).as_ref(), self.connected_key().as_ref(), screen, &self.staged)
    }

    /// The database on screen, if one is connected.
    pub(crate) fn connected_key(&self) -> Option<DbKey> {
        match &self.phase {
            Phase::Connected { conn, .. } => Some(DbKey::of_conn(conn)),
            _ => None,
        }
    }

    /// The database `leaving` leaves: for a switch the connected one, if
    /// any; for a stop or a removal the row's. A quit leaves every one, and
    /// reads as the connected one here.
    fn left_by(&self, leaving: &Leaving) -> Option<DbKey> {
        match leaving {
            Leaving::Quit | Leaving::Relaunch | Leaving::Switch { .. } => self.connected_key(),
            Leaving::Stop { name, path } => Some(self.key_of(name, Some(path))),
            Leaving::Remove { name } => Some(DbKey::Remote(clone_str(name))),
        }
    }

    /// Before the connected database is left (`Leaving::Switch`, or a Stop
    /// or Remove of it), text in an open cell editor is staged, to be
    /// parked with the rest. False when its column refuses it: the editor
    /// stays open with the reason, and nothing is left.
    pub(crate) fn settle_before(&mut self, leaving: &Leaving, cx: &mut Context<Self>) -> bool {
        if matches!(leaving, Leaving::Quit | Leaving::Relaunch) {
            return true;
        }
        let db = self.left_by(leaving);
        match self.grid.clone() {
            Some(grid) if db.is_some() && db == self.connected_key() => grid.update(cx, |g, cx| g.settle_editor(cx)),
            _ => true,
        }
    }

    /// Go ahead with what the one dialog asked about, or what needed no
    /// asking: leave the connected database for another, stop a server, or
    /// remove a saved remote. A quit is main.rs's to carry out. A switch
    /// replaces whatever the dialog held back; anything else resumes it,
    /// as a Cancel would.
    pub(crate) fn go_ahead(&mut self, leaving: Leaving, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(leaving, Leaving::Quit | Leaving::Relaunch) {
            return;
        }
        let asked = std::mem::take(&mut self.asking_to_quit);
        match leaving {
            Leaving::Quit | Leaving::Relaunch => {}
            Leaving::Switch { to, .. } => {
                self.called_off = None;
                self.deferred_select = None;
                self.dial(to, window, cx);
                return;
            }
            Leaving::Stop { name, path } => self.stop_berth(name, path, cx),
            Leaving::Remove { name } => self.remove_remote_database(name, cx),
        }
        if asked {
            self.resume(window, cx);
        }
    }

    /// The one dialog opens. A connect still in flight is called off: its
    /// landing would replace the grid, the query and every staged edit
    /// under the dialog, and Cancel must find them as they were. What it
    /// was aimed at is kept, and dialed again if the dialog is cancelled.
    pub(crate) fn quit_dialog_opened(&mut self, cx: &mut Context<Self>) {
        self.asking_to_quit = true;
        self.called_off = self.connecting_aim.take();
        if self.called_off.is_some() {
            self.cancel(cx);
        }
    }

    /// The one dialog was cancelled. What waited for it runs (`resume`).
    pub(crate) fn quit_dialog_cancelled(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.asking_to_quit = false;
        self.resume(window, cx);
    }

    /// What the dialog held back runs (`after_quit_dialog`), and the fleet
    /// is reconciled, which drops a connection whose server stopped
    /// meanwhile. A connect called off is a switch like any other: when the
    /// database on screen holds something it would end, it asks first.
    fn resume(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match after_quit_dialog(self.called_off.take(), self.deferred_select.take()) {
            Resume::Dial(aim) => crate::leave_asking(self.switch_to(aim), cx),
            Resume::Select(rel) => self.select_table(rel, window, cx),
            Resume::Nothing => {}
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

    /// The app is ending (`on_app_quit`): give back every session this
    /// window holds, so the server ends what runs on them at once instead
    /// of at their timeouts. They are the Query view's transaction, open or
    /// still opening, and the grid's commit in flight. Each release is one
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
    pub(crate) fn select_table(&mut self, rel: Rel, window: &mut Window, cx: &mut Context<Self>) {
        let (db, conn, solo_schema, structure) = match &self.phase {
            Phase::Connected { conn, catalog, .. } => (
                DbKey::of_conn(conn),
                conn.clone(),
                catalog.schemas().len() <= 1,
                crate::structure::structure_of(catalog, &rel),
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
        // Until the page lands, no editor opens on the grid it replaces.
        if let Some(grid) = &self.grid {
            grid.update(cx, |grid, _| grid.replacing = true);
        }
        // "main.tests" earns its prefix only when there is another schema
        // to distinguish it from, and an attached catalog's table its
        // catalog's name.
        let title = rel.title(solo_schema);
        let previous = self.selected_table.replace(rel.clone());
        self.select_seq += 1;
        let fence = self.select_seq;
        let page_size = crate::prefs::get(cx).page_size;
        // The table opens under the sort it had this session, while it has
        // the columns the sort names, with its ties broken as the grid
        // breaks them.
        let source = rel.source();
        let columns: Vec<String> =
            structure.iter().flat_map(|s| s.cols.iter().map(|c| c.name.clone())).collect();
        let sort = crate::grid::remembered_sort(
            self.sorts.get(&(db.clone(), clone_str(&source))).cloned().unwrap_or_default(),
            &columns,
        );
        let tiebreak =
            crate::sql::tiebreak(&crate::grid::identity_columns(structure.as_ref()), &columns);
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            // Two independent queries (each on its own connection), so they
            // run concurrently — click latency is the slower one, not the
            // sum. The structure came free out of the catalog snapshot. A
            // failed page drops the count unawaited.
            // Keyless tables fetch DuckDB's implicit rowid as their
            // editing identity — the same predicate Grid::new applies.
            let rowid = structure.as_ref().is_some_and(|s| s.keyed_by_rowid());
            // A view is not counted: counting a view over a file reads the
            // whole file, and the grid pages without a total.
            let counted = !structure.as_ref().is_some_and(|s| s.view);
            let page_task = cx.background_executor().spawn({
                let (conn, source) = (conn.clone(), clone_str(&source));
                async move { crate::sql::first_page(&conn, &source, rowid, sort, &tiebreak, page_size) }
            });
            let total_task = cx.background_executor().spawn({
                let (conn, source) = (conn.clone(), clone_str(&source));
                async move { if counted { crate::sql::total_rows(&conn, &source) } else { None } }
            });
            let (outcome, sort) = page_task.await;
            let total = if outcome.is_ok() { total_task.await } else { None };
            this.update_in(cx, |state, window, cx| {
                if state.select_seq != fence {
                    return;
                }
                if !matches!(state.phase, Phase::Connected { .. }) {
                    return;
                }
                // A switch that would land during a commit, or under the
                // quit dialog, waits for it, and the grid on screen is
                // itself again meanwhile.
                if state.asking_to_quit || state.grid.as_ref().is_some_and(|g| g.read(cx).committing) {
                    state.deferred_select = Some(rel.clone());
                    if let Some(grid) = &state.grid {
                        grid.update(cx, |grid, _| grid.replacing = false);
                    }
                    return;
                }
                // No editor opens on the outgoing grid while the page is on
                // its way. Were one open, its text would be neither
                // confirmed unasked nor parked: the switch yields, and the
                // sidebar goes back to that table.
                if let Some(old) = &state.grid
                    && old.read(cx).is_editing()
                {
                    old.update(cx, |grid, _| grid.replacing = false);
                    state.selected_table = previous;
                    cx.notify();
                    return;
                }
                // Staged edits outlive the grid that collected them (Law
                // 4): park the outgoing table's, keyed by its database and
                // source, before the swap discards its view.
                if let Some(old) = state.grid.take()
                    && let Some(edits) = old.update(cx, |g, _| g.take_edits())
                {
                    state.staged.park(db.clone(), edits);
                }
                // A sort the first page could not be read under is gone
                // from the table's memory too.
                let key = (db.clone(), clone_str(&source));
                if sort.is_empty() {
                    state.sorts.remove(&key);
                }
                // The Data/Structure choice is a browsing mode, not table
                // state (prefs.view): it survives this table switch.
                let grid = cx.new(|cx| {
                    crate::grid::Grid::new(
                        conn, clone_str(&source), title, outcome, total, page_size, structure, sort,
                        window, cx,
                    )
                });
                cx.subscribe(&grid, |state, _, _: &CatalogRefreshRequested, cx| {
                    state.refresh_catalog(cx)
                })
                .detach();
                cx.subscribe(&grid, move |state, _, e: &crate::grid::SortChanged, _| {
                    if e.0.is_empty() {
                        state.sorts.remove(&key);
                    } else {
                        state.sorts.insert(key.clone(), e.0.clone());
                    }
                })
                .detach();
                cx.subscribe_in(&grid, window, |state, _, _: &CommitSettled, window, cx| {
                    // Under the quit dialog the switch keeps waiting: taken
                    // here it would be refused by the dialog and lost, the
                    // sidebar on one table and the grid on another. It runs
                    // when the dialog is cancelled.
                    if state.asking_to_quit {
                        return;
                    }
                    if let Some(rel) = state.deferred_select.take() {
                        state.select_table(rel, window, cx);
                    }
                })
                .detach();
                // And returning to a table hands its parked edits back.
                // The grid owns them from here: one whose first fetch
                // failed has no columns to judge them by, keeps them until
                // a fetch brings some, and surrenders them again through
                // `take_edits` if it is replaced first.
                if let Some(stash) = state.staged.take(&db, &source) {
                    grid.update(cx, |g, cx| g.adopt_edits(stash, cx));
                }
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

    /// ⌥←/⌥→: the previous/next table, walking the sidebar's own order
    /// and filter (sidebar.rs visible_tables) — with rollover, so the
    /// tables read as a ring you can circle rather than a hall that
    /// dead-ends. With no table selected, either arrow lands on the
    /// nearest end.
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
        self.select_table(list[next].clone(), window, cx);
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
        self.catalog_seq += 1;
        let connection = self.connection;
        let refresh = self.catalog_seq;
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move { read_catalog(&conn) })
                .await;
            this.update(cx, |state, cx| {
                if !catalog_refresh_is_current(state.connection, state.catalog_seq, connection, refresh) {
                    return;
                }
                match outcome {
                    Ok(new_catalog) => {
                        if let Phase::Connected { catalog, .. } = &mut state.phase {
                            **catalog = new_catalog;
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
        let connected: Option<(DbKey, usize, Option<bool>, bool)> = match &self.phase {
            Phase::Connected { conn, catalog, .. } => {
                Some((DbKey::of_conn(conn), relations(catalog), conn.tunnel_up(), answering(self.unanswered)))
            }
            _ => None,
        };
        let conn = match &self.phase {
            Phase::Connected { conn, .. } => Some(conn.clone()),
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
                        let here = known.filter(|(connected, ..)| *connected == key);
                        let (tunnel_up, answering) = here.as_ref().map_or((None, true), |(.., up, answering)| (*up, *answering));
                        let tables = match here {
                            Some((_, count, ..)) => Some(count),
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
                                    Some(relations(&cat))
                                })
                                .flatten(),
                        };
                        RowVm {
                            // A server that exited under the connection, or its
                            // tunnel, shows as stopped, and the reconciliation
                            // below lets the connection go. One that does not
                            // answer through its tunnel shows as stopped too.
                            state: shown_state(row.state, tunnel_up, answering),
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
                            workspace: row.workspace,
                        }
                    })
                })
                .collect();
            let mut rows = Vec::with_capacity(tasks.len());
            for task in tasks {
                rows.push(task.await);
            }
            // The connection on screen, if its server is gone (`lost`). One
            // that a busy server's missed survey would have dropped is asked
            // again on its own transport, and its row shows what it answers.
            let lost = match (connected, conn) {
                (Some((key, _, tunnel_up, _)), Some(conn)) => {
                    let surveyed = rows.iter().any(|r| r.key == key && r.state.is_live());
                    let ready = move || conn.transport().is_ok_and(harbor_client::http::ready);
                    let gone = cx.background_executor().spawn(async move { lost(surveyed, tunnel_up, ready) }).await;
                    if !gone && !surveyed && tunnel_up.is_none() {
                        rows.iter_mut().filter(|r| r.key == key).for_each(|r| r.state = State::Running);
                    }
                    gone.then_some(key)
                }
                _ => None,
            };
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
                // Reconcile the connection: one whose server exited out from
                // under it (`lost`) is dropped cleanly with the way back
                // pointed out, rather than left to fail the next catalog or
                // query with an OS error. A connection made since the survey
                // began is not the one it judged.
                let connected = match &state.phase {
                    Phase::Connected { conn, .. } => {
                        Some((clone_str(&conn.name), DbKey::of_conn(conn), conn.tunnel_up().is_some()))
                    }
                    _ => None,
                };
                // Under the quit dialog the connection is left as it is:
                // dropping it takes the grid and the Query view down, and
                // Cancel must find them where they were. The refresh that
                // follows a cancel reconciles.
                if let Some((name, key, tunneled)) = connected {
                    if !state.asking_to_quit && lost.as_ref() == Some(&key) {
                        state.drop_connection(cx);
                        let gone = if tunneled { "lost its SSH tunnel" } else { "stopped" };
                        state.warning = Some(format!("{name} {gone} — click it to reconnect"));
                    } else if tunneled && !answering(state.unanswered) {
                        state.warning = Some(format!("{name} is not answering over SSH"));
                    }
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
    pub(crate) fn outdated(&self) -> impl Iterator<Item = &RowVm> {
        let installed = self.installed_version.as_deref();
        self.rows.iter().filter(move |r| installed.is_some_and(|v| r.upgradable(v)))
    }

    /// Upgrade every outdated local server: restart each onto the installed
    /// binary in the mode it was running, then refresh so the badge clears as
    /// they come back current. Runs on a background thread; the first failure
    /// is surfaced, the rest still attempted.
    pub(crate) fn upgrade_outdated(&mut self, cx: &mut Context<Self>) {
        let targets: Vec<(std::path::PathBuf, bool)> =
            self.outdated().filter_map(|r| r.path.clone().map(|p| (p, r.ephemeral))).collect();
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
        let n = self.outdated().count();
        if n == 0 {
            return;
        }
        let installed = self.installed_version.clone().unwrap_or_default();
        let noun = if n == 1 { "database".to_string() } else { format!("{n} databases") };
        let title = format!("Upgrade {noun}");
        let mut body = format!(
            "Restart {noun} onto harbor {installed}. Each server stops and comes \
             back in the same mode; any connected clients reconnect."
        );
        // The database on screen among them: what its restart ends is said.
        let connected = self.connected_key();
        if let Some(row) = self.outdated().find(|r| Some(&r.key) == connected.as_ref())
            && let Some(path) = row.path.clone()
        {
            let leaving = Leaving::Stop { name: clone_str(&row.name), path };
            for note in restart_notes(&row.name, &self.risks(&leaving, cx)) {
                body.push(' ');
                body.push_str(&note);
            }
        }
        let answer =
            window.prompt(PromptLevel::Info, &title, Some(&body), &["Upgrade", "Cancel"], cx);
        cx.spawn(async move |this, cx| {
            if answer.await == Ok(0) {
                this.update(cx, |state, cx| state.upgrade_outdated(cx)).ok();
            }
        })
        .detach();
    }

    /// A click on a sidebar row. The database already on screen stays as it
    /// is, and a connect in flight to another is called off; any other is
    /// opened, asking first when leaving the connected one would lose
    /// something (`Leaving::Switch`).
    pub(crate) fn choose_row(&mut self, name: String, path: Option<std::path::PathBuf>, cx: &mut Context<Self>) {
        if self.connected_key() == Some(self.key_of(&name, path.as_deref())) {
            if self.connecting.is_some() {
                self.cancel(cx);
            }
            return;
        }
        crate::leave_asking(self.switch_to(Aim::Row { name, path }), cx);
    }

    /// Leaving the connected database, if any, for `to`.
    pub(crate) fn switch_to(&self, to: Aim) -> Leaving {
        let from = match &self.phase {
            Phase::Connected { conn, .. } => clone_str(&conn.name),
            _ => String::new(),
        };
        Leaving::Switch { from, to }
    }

    /// Dial again what a failed connect was aimed at.
    pub(crate) fn retry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Phase::Failed { aim, .. } = &self.phase {
            self.dial(aim.clone(), window, cx);
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

    /// File → Open Database URL's OK: open the address, asking first when
    /// leaving the connected database would lose something.
    pub(crate) fn open_url(&mut self, name: String, host: String, port: String, cx: &mut Context<Self>) {
        let name = harbor_client::paths::normalize(&name).unwrap_or(name);
        crate::leave_asking(self.switch_to(Aim::Url { name, host, port }), cx);
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
        // The database is forgotten, and what was staged for it with it,
        // once the config has let it go: the one dialog asked first
        // (`Leaving::Remove`). A removal the config refuses keeps both.
        let db = DbKey::Remote(clone_str(&name));
        self.fleet_then(move || fleet::remove_remote(&name), move |state| state.staged.forget(&db), cx);
    }

    /// Connect to what `aim` names: the current content keeps rendering
    /// while the connect runs on a background thread (raise or join the
    /// server, read its catalog), and the whole pane swaps to the outcome in
    /// one frame (the fetch-first rule of `select_table`: a click never
    /// flashes an intermediate state). The name shows on the sidebar row
    /// meanwhile; the idle and failed cards give way to a connecting card,
    /// since they hold nothing worth keeping. A stale fence discards itself,
    /// so a slow attempt never clobbers a newer one.
    ///
    /// A sidebar row connects to what it shows: its file when it has one,
    /// the config's remote of its name otherwise, never the name looked up
    /// again, because a file and a remote can share one. A file opened or
    /// dropped needs no config entry: the path is the target, and the
    /// refresh that follows shows the server under its own `/info` name.
    /// Open Database URL saves the address under its name and connects to
    /// that remote; dialed again (Retry, or Cancel on the one dialog that
    /// called it off), it saves again, since an earlier dial may or may not
    /// have saved it.
    fn dial(&mut self, aim: Aim, window: &mut Window, cx: &mut Context<Self>) {
        // The one dialog promises that Cancel leaves everything as it was. A
        // connect replaces the grid, the query and the staged edits on
        // screen, and the menu bar and a file drop still reach it under the
        // dialog.
        if self.asking_to_quit {
            return;
        }
        self.attempt += 1;
        let fence = self.attempt;
        let shown = match &aim {
            Aim::Row { name, .. } | Aim::Url { name, .. } => clone_str(name),
            Aim::File(path) => path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            Aim::Workspace(_) => "Workspace".to_string(),
        };
        self.connecting = Some(clone_str(&shown));
        self.connecting_key = Some(match &aim {
            Aim::Row { name, path } => self.key_of(name, path.as_deref()),
            Aim::File(_) | Aim::Workspace(_) => match aim.file() {
                Some(path) => self.key_of("", Some(&path)),
                None => DbKey::Remote(String::new()),
            },
            Aim::Url { name, .. } => DbKey::Remote(clone_str(name)),
        });
        self.connecting_aim = Some(aim.clone());
        // What the database on screen holds at risk as the connect begins:
        // nothing, or what the dialog asked about and was told to leave.
        let accepted = self.risks(&self.switch_to(aim.clone()), cx);
        cx.notify();
        let target = aim.clone();
        cx.spawn_in(window, async move |this, cx| {
            // A file opened by one spelling of its path may have a row under
            // another (/tmp and /private/tmp): the row to light is found by
            // the canonical path, which is read off this thread.
            if let Some(path) = target.file() {
                let key = cx.background_executor().spawn(async move { DbKey::of_file(&path) }).await;
                this.update(cx, |state, cx| {
                    if state.attempt == fence {
                        state.connecting_key = Some(key);
                        cx.notify();
                    }
                })
                .ok();
            }
            let outcome = cx.background_executor().spawn(async move { connect(target) }).await;
            this.update_in(cx, |state, window, cx| {
                if state.attempt != fence {
                    return;
                }
                state.connecting = None;
                state.connecting_key = None;
                state.connecting_aim = None;
                // The landing replaces the grid and the Query view, and a
                // failed one too. Something put at risk while the connect
                // ran (a BEGIN typed, a statement started) is asked about
                // first, as a fresh switch: this outcome is let go, and
                // Leave Anyway dials again.
                let leaving = state.switch_to(aim.clone());
                if state.risks(&leaving, cx).since(&accepted).question_for(&leaving).is_some() {
                    cx.notify();
                    crate::leave_asking(leaving, cx);
                    return;
                }
                state.selected_table = None;
                state.park_grid(cx);
                state.deferred_select = None;
                state.select_seq += 1;
                state.connection += 1;
                state.unanswered = 0;
                // The database's one Query scratchpad (docs/QUERY.md law 1)
                // is there as soon as it connects, tables or none.
                state.query = outcome.as_ref().ok().map(|(conn, info, ..)| {
                    let (conn, name) = (conn.clone(), clone_str(&info.name));
                    let query = cx.new(|cx| crate::query::QueryView::new(conn, &name, window, cx));
                    cx.subscribe(&query, |state, _, _: &CatalogRefreshRequested, cx| state.refresh_tables(cx))
                        .detach();
                    query
                });
                let mut opened = Vec::new();
                state.phase = match outcome {
                    Ok((conn, info, catalog, openings)) => {
                        opened = openings;
                        Phase::Connected { conn, info, catalog: Box::new(catalog) }
                    }
                    Err(message) => Phase::Failed { name: clone_str(&shown), message, aim },
                };
                state.sync_path_copy(cx);
                state.watch_tunnel(cx);
                state.refresh(cx);
                state.opened(opened, window, cx);
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

    /// The grid goes with its connection, and its staged set is parked for
    /// its database (`Grid::surrender_edits`), to come back when that
    /// database is opened again. Text in an open editor is staged first;
    /// text its column refuses goes with the grid.
    fn park_grid(&mut self, cx: &mut Context<Self>) {
        let Some(grid) = self.grid.take() else { return };
        let Phase::Connected { conn, .. } = &self.phase else { return };
        let db = DbKey::of_conn(conn);
        let edits = grid.update(cx, |g, cx| {
            g.settle_editor(cx);
            g.surrender_edits()
        });
        if let Some(edits) = edits {
            self.staged.park(db, edits);
        }
    }

    /// Ask the tunneled remote on screen `/ready` through its tunnel every
    /// `PROBE_EVERY`: its SSH process can outlive a server that exits on its
    /// machine, and the row would show green until a request failed. A
    /// change either way refreshes the list, which draws the row from
    /// `unanswered` (`shown_state`), and an SSH process found exited is
    /// lost there, as a refresh finds it. Nothing is asked while a
    /// statement, a page, a commit or an export is on the connection: what
    /// it brings back says whether the server answers, and a probe beside
    /// it would be slowed by it. One watcher per connection; it ends with it.
    fn watch_tunnel(&mut self, cx: &mut Context<Self>) {
        let Phase::Connected { conn, .. } = &self.phase else { return };
        if conn.tunnel_up().is_none() {
            return;
        }
        let connection = self.connection;
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(PROBE_EVERY).await;
            let asked = this.update(cx, |state, cx| match &state.phase {
                Phase::Connected { conn, .. } if state.connection == connection => {
                    Some((!state.busy(cx)).then(|| conn.clone()))
                }
                _ => None,
            });
            let conn = match asked {
                Ok(Some(Some(conn))) => conn,
                Ok(Some(None)) => continue,
                _ => break,
            };
            let (answered, tunnel_up) = cx
                .background_executor()
                .spawn(async move { (conn.answers(PROBE_PATIENCE), conn.tunnel_up()) })
                .await;
            let watching = this.update(cx, |state, cx| {
                if state.connection != connection {
                    return false;
                }
                let was = answering(state.unanswered);
                state.unanswered = if answered { 0 } else { state.unanswered.saturating_add(1) };
                if answering(state.unanswered) != was || tunnel_up == Some(false) {
                    state.refresh(cx);
                }
                true
            });
            if !matches!(watching, Ok(true)) {
                break;
            }
        })
        .detach();
    }

    /// Whether a request is on the connection on screen: a statement in the
    /// Query view, a page or a commit in the grid, or an export.
    fn busy(&self, cx: &App) -> bool {
        self.query.as_ref().is_some_and(|q| q.read(cx).is_running())
            || self.grid.as_ref().is_some_and(|g| {
                let grid = g.read(cx);
                grid.committing || grid.table_facts(cx).2
            })
            || crate::export::running(cx)
    }

    /// Clear the connected world back to Idle, forgetting its table, grid
    /// and query; its staged edits are parked for the database
    /// (`park_grid`). Shared by Stop and by refresh's reconciliation when
    /// the server exits out from under us — either way there is nothing
    /// left to show, and a lingering dead connection would only fail the next
    /// catalog or query with a raw OS error.
    fn drop_connection(&mut self, cx: &mut Context<Self>) {
        self.park_grid(cx);
        self.connection += 1;
        self.unanswered = 0;
        self.phase = Phase::Idle;
        self.selected_table = None;
        self.query = None;
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
        self.fleet_then(op, |_| {}, cx);
    }

    /// `fleet_then_refresh`, with `done` run on the app once the call has
    /// succeeded.
    fn fleet_then(
        &self,
        op: impl FnOnce() -> Result<(), String> + Send + 'static,
        done: impl FnOnce(&mut Self) + 'static,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let outcome = cx.background_executor().spawn(async move { op() }).await;
            this.update(cx, |state, cx| {
                match outcome {
                    Ok(()) => done(state),
                    Err(message) => state.warning = Some(message),
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

    /// Open what `plan` says (`open::plan`): files that open in the
    /// workspace, in it, and a DuckDB database as itself. Opening switches
    /// to the workspace, or to the database, as any switch does, asking
    /// first when leaving the database on screen would end something
    /// (`Leaving::Switch`); on the workspace already, the files open in it
    /// where it is. `workspace` is the workspace's key, read off this
    /// thread.
    pub(crate) fn open_planned(&mut self, plan: crate::open::Plan, workspace: Option<DbKey>, cx: &mut Context<Self>) {
        if !plan.workspace.is_empty() {
            let here = workspace.is_some() && self.connected_key() == workspace && self.connecting.is_none();
            match here {
                true => crate::on_view_in(cx, move |app, window, cx| app.open_in_workspace(plan.workspace, window, cx)),
                false => crate::leave_asking(self.switch_to(Aim::Workspace(plan.workspace)), cx),
            }
        } else if let Some(db) = plan.database {
            crate::leave_asking(self.switch_to(Aim::File(db)), cx);
        }
    }

    /// Open `files` in the workspace on screen, then read its catalog again
    /// and show the first of them (`opened`).
    pub(crate) fn open_in_workspace(
        &mut self,
        files: Vec<(std::path::PathBuf, Kind)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Under the quit dialog nothing replaces what is behind it.
        if self.asking_to_quit {
            return;
        }
        self.on_connection(move |conn| open_files(conn, &files), |state, opened, window, cx| state.opened(opened, window, cx), window, cx);
    }

    /// Files have opened in the workspace on screen: the first of them is
    /// shown, the status line says what opened, and an alert what did not.
    fn opened(&mut self, opened: Openings, window: &mut Window, cx: &mut Context<Self>) {
        if opened.is_empty() {
            return;
        }
        let (said, refused) = crate::open::report(&opened);
        if let Some(said) = said {
            crate::footer::say(cx, said, false, Some(std::time::Duration::from_secs(8)));
        }
        if !refused.is_empty() {
            crate::open::alert("DuckTable could not open everything", refused.join("\n\n"), cx);
        }
        let first = match &self.phase {
            Phase::Connected { catalog, .. } => crate::open::first_opened(catalog, &opened),
            _ => None,
        };
        if let Some(rel) = first {
            self.select_table(rel, window, cx);
        }
    }

    /// Run `work` on the connection on screen off this thread, read the
    /// catalog again, and swap it in; then `done`, with what `work` gave
    /// back. A connection gone or replaced meanwhile drops it all. Opening,
    /// importing, removing and detaching each change the catalog, and the
    /// sidebar shows the change before anything is selected in it.
    fn on_connection<T: Send + 'static>(
        &mut self,
        work: impl FnOnce(&Conn) -> T + Send + 'static,
        done: impl FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Phase::Connected { conn, .. } = &self.phase else { return };
        let conn = conn.clone();
        let connection = self.connection;
        self.catalog_seq += 1;
        let refresh = self.catalog_seq;
        cx.spawn_in(window, async move |this, cx| {
            let (outcome, catalog) = cx
                .background_executor()
                .spawn(async move {
                    let outcome = work(&conn);
                    (outcome, read_catalog(&conn))
                })
                .await;
            this.update_in(cx, |state, window, cx| {
                if state.connection != connection {
                    return;
                }
                if let (Ok(new_catalog), true) = (catalog, state.catalog_seq == refresh)
                    && let Phase::Connected { catalog, .. } = &mut state.phase
                {
                    **catalog = new_catalog;
                }
                done(state, outcome, window, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Import as Table (docs/OPEN.md): copy a workspace view's rows into a
    /// table of its name, editable, and drop the view. A file larger than
    /// `open::ASK_ABOVE` asks first, saying its size.
    pub(crate) fn import_view(&mut self, rel: Rel, window: &mut Window, cx: &mut Context<Self>) {
        if self.asking_to_quit {
            return;
        }
        let Phase::Connected { catalog, .. } = &self.phase else { return };
        let Some(view) = catalog.views.iter().find(|v| rel.catalog.is_none() && v.schema == rel.schema && v.name == rel.name)
        else {
            return;
        };
        let source = view.ddl.as_deref().and_then(harbor_client::workspace::view_source);
        cx.spawn_in(window, async move |this, cx| {
            let measured = source.clone();
            let bytes = cx
                .background_executor()
                .spawn(async move { measured.as_deref().and_then(harbor_client::workspace::size_on_disk) })
                .await;
            let question = source.as_deref().and_then(|s| crate::open::import_question(&rel.name, s, bytes));
            let go = match question {
                None => true,
                Some((message, detail)) => {
                    let answer = this
                        .update_in(cx, |_, window, cx| {
                            window.prompt(PromptLevel::Info, &message, Some(&detail), &["Import", "Cancel"], cx)
                        })
                        .ok();
                    match answer {
                        Some(answer) => answer.await == Ok(0),
                        None => false,
                    }
                }
            };
            if go {
                this.update_in(cx, |state, window, cx| state.run_import(rel, window, cx)).ok();
            }
        })
        .detach();
    }

    fn run_import(&mut self, rel: Rel, window: &mut Window, cx: &mut Context<Self>) {
        if self.asking_to_quit {
            return;
        }
        // The word holds until the import's own replaces it.
        crate::footer::say(cx, format!("importing {}\u{2026}", rel.name), false, None);
        let (schema, name) = (clone_str(&rel.schema), clone_str(&rel.name));
        self.on_connection(
            move |conn| harbor_client::workspace::import(conn, &schema, &name),
            move |state, outcome, window, cx| {
                match outcome {
                    Ok(rows) => {
                        let rows = format!("{} {}", crate::util::commas(rows), if rows == 1 { "row" } else { "rows" });
                        crate::footer::say(cx, format!("imported {rows} into the table {}", rel.name), false, Some(std::time::Duration::from_secs(8)));
                        // The grid on the view is read-only: the table's is not.
                        if state.selected_table.as_ref() == Some(&rel) {
                            state.select_table(rel, window, cx);
                        }
                    }
                    Err(why) => {
                        let word = format!("{} was not imported", rel.name);
                        crate::footer::say(cx, clone_str(&word), true, Some(std::time::Duration::from_secs(20)));
                        crate::open::alert(word, why, cx);
                    }
                }
            },
            window,
            cx,
        );
    }

    /// Remove a workspace view: drop it, leaving the file it reads as it
    /// is.
    pub(crate) fn remove_view(&mut self, rel: Rel, window: &mut Window, cx: &mut Context<Self>) {
        if self.asking_to_quit {
            return;
        }
        let (schema, name) = (clone_str(&rel.schema), clone_str(&rel.name));
        self.on_connection(
            move |conn| harbor_client::workspace::remove_view(conn, &schema, &name),
            move |state, outcome, _, cx| match outcome {
                Ok(()) => state.let_go_of(|selected| selected == &rel, cx),
                Err(why) => crate::open::alert(format!("{} was not removed", rel.name), why, cx),
            },
            window,
            cx,
        );
    }

    /// Detach a catalog, leaving its file as it is, and in the workspace
    /// let go of its record, so the workspace does not attach it again.
    /// The `DETACH` goes whether or not the catalog lists it with an error
    /// (`workspace::detach_statements`). What was staged on its tables is
    /// parked with the rest of the database's.
    pub(crate) fn detach_catalog(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.asking_to_quit {
            return;
        }
        let detached = clone_str(&name);
        let workspace = self.on_workspace();
        self.on_connection(
            move |conn| harbor_client::workspace::detach(conn, &detached, workspace),
            move |state, outcome, _, cx| match outcome {
                Ok(()) => state.let_go_of(|selected| selected.catalog.as_deref() == Some(name.as_str()), cx),
                Err(why) => crate::open::alert(format!("{name} was not detached"), why, cx),
            },
            window,
            cx,
        );
    }

    /// The table on screen is gone from the catalog when `gone` says so:
    /// its grid goes, its staged set parked, and nothing is selected.
    fn let_go_of(&mut self, gone: impl Fn(&Rel) -> bool, cx: &mut Context<Self>) {
        if self.selected_table.as_ref().is_some_and(&gone) {
            self.park_grid(cx);
            self.selected_table = None;
            self.select_seq += 1;
        }
    }

    /// Abort the in-flight connect. The current phase never changed, so
    /// whatever was on screen simply stays (a cancelled connect is not a
    /// failed connect).
    pub(crate) fn cancel(&mut self, cx: &mut Context<Self>) {
        self.attempt += 1;
        self.connecting = None;
        self.connecting_key = None;
        self.connecting_aim = None;
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
    use super::{active_key, after_quit_dialog, catalog_refresh_is_current, shown_state};
    use super::{Aim, DbKey, QuitRisks, Rel, Resume, State};

    #[test]
    fn a_row_shows_the_survey_unless_it_is_the_tunnel_on_screen() {
        // A file, or a remote on this machine: the survey's probe stands,
        // connected or not, so a server gone under the connection is stopped.
        assert_eq!(shown_state(State::Stopped, None, true), State::Stopped);
        assert_eq!(shown_state(State::Running, None, false), State::Running);
        // The tunneled remote on screen, which the survey never dials: its
        // SSH process says whether it is up, and its server whether it
        // answers through it.
        assert_eq!(shown_state(State::Stopped, Some(true), true), State::Running);
        assert_eq!(shown_state(State::Stopped, Some(false), true), State::Stopped);
        assert_eq!(shown_state(State::Running, Some(true), false), State::Stopped);
    }

    #[test]
    fn a_tunneled_server_shows_dim_only_after_misses_in_a_row() {
        use super::{answering, MISSES, PROBE_EVERY};
        // One miss, a slow link's, is not enough; the second in a row is.
        assert!(answering(0));
        assert!(answering(1));
        assert!(!answering(2));
        // Gone, it shows dim within about thirty seconds.
        assert!(PROBE_EVERY * MISSES <= std::time::Duration::from_secs(30));
    }

    #[test]
    fn a_missed_survey_drops_a_connection_only_when_its_server_fails_ready_too() {
        use super::lost;
        let asked = std::cell::Cell::new(0);
        let answers = |up: bool| {
            let asked = &asked;
            move || {
                asked.set(asked.get() + 1);
                up
            }
        };
        // Seen running: nothing is asked.
        assert!(!lost(true, None, answers(false)));
        assert_eq!(asked.get(), 0);
        // Missed by a survey, yet answering on its own transport: kept.
        assert!(!lost(false, None, answers(true)));
        // Missed, and not answering either: gone.
        assert!(lost(false, None, answers(false)));
        assert_eq!(asked.get(), 2);
        // A tunnel whose SSH process exited is gone without asking.
        assert!(lost(false, Some(false), answers(true)));
        // One whose SSH process runs is kept, its server answering or not:
        // its row shows which, and it comes back when the server does.
        assert!(!lost(false, Some(true), answers(false)));
        assert_eq!(asked.get(), 2);
    }

    #[test]
    fn a_cancelled_quit_dialog_resumes_what_it_held_back() {
        let row = Aim::Row { name: "a".into(), path: Some("/data/a.duckdb".into()) };
        let switch = Some(Rel::own("main", "orders"));
        // A connect the dialog called off is dialed again, to the same aim.
        assert_eq!(after_quit_dialog(Some(row.clone()), None), Resume::Dial(row.clone()));
        let file = Aim::File("/tmp/x.duckdb".into());
        assert_eq!(after_quit_dialog(Some(file.clone()), None), Resume::Dial(file));
        // An Open Database URL dial is dialed again as itself: it saves the
        // address again, not a plain connect to a name that may not be saved.
        let url = Aim::Url { name: "prod".into(), host: "db.example".into(), port: "9495".into() };
        assert_eq!(after_quit_dialog(Some(url.clone()), None), Resume::Dial(url));
        // A table switch that waited under it runs.
        assert_eq!(
            after_quit_dialog(None, switch.clone()),
            Resume::Select(Rel::own("main", "orders"))
        );
        // With both, the connect: it replaces the grid the switch was for.
        assert_eq!(after_quit_dialog(Some(row.clone()), switch), Resume::Dial(row));
        assert_eq!(after_quit_dialog(None, None), Resume::Nothing);
    }
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
            exporting: false,
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
        // So is an export writing its file: the quit stops it.
        let exporting = QuitRisks { exporting: true, ..Default::default() }.question().unwrap();
        assert_eq!((exporting.message.as_str(), exporting.confirm), ("Quit DuckTable?", "Quit Anyway"));
        assert_eq!(exporting.detail, "An export is still writing its file. Quitting stops it, and no file is written.");
        let staged = QuitRisks { staged: 1, tables: 1, exporting: true, ..Default::default() }.question().unwrap();
        assert_eq!((staged.message.as_str(), staged.confirm), ("Quit DuckTable?", "Quit Anyway"));
    }

    #[test]
    fn installing_an_update_asks_what_quitting_asks() {
        use super::Leaving::Relaunch;
        // Nothing to lose, nothing asked: the relaunch goes ahead.
        assert_eq!(QuitRisks::default().question_for(&Relaunch), None);

        let one = QuitRisks { staged: 1, tables: 1, ..Default::default() }.question_for(&Relaunch).unwrap();
        assert_eq!(one.message, "Discard 1 staged change and install the update?");
        assert_eq!(
            one.detail,
            "1 staged change has not been committed, and quitting discards it. Installing the \
             update quits DuckTable. With Cancel it installs when DuckTable next quits."
        );
        assert_eq!(one.confirm, "Discard and Install");

        let open = QuitRisks { transaction: true, ..Default::default() }.question_for(&Relaunch).unwrap();
        assert_eq!((open.message.as_str(), open.confirm), ("Install the update now?", "Install Anyway"));
        assert!(open.detail.starts_with("The Query view holds a transaction open, and quitting rolls it back."));
    }

    #[test]
    fn leaving_a_database_asks_only_about_what_leaving_it_loses() {
        use super::Leaving;
        let switch = Leaving::Switch { from: "orders".into(), to: Aim::File("/tmp/x.duckdb".into()) };
        let stop = Leaving::Stop { name: "orders".into(), path: "/data/orders.duckdb".into() };
        let remove = Leaving::Remove { name: "orders".into() };
        let staged = QuitRisks { staged: 3, tables: 2, held: 1, ..Default::default() };

        // A switch parks staged and held sets for the database's return:
        // they are no reason to ask.
        assert_eq!(staged.question_for(&switch), None);
        // Stop keeps them parked too, and asks, saying so.
        let stopping = staged.question_for(&stop).unwrap();
        assert_eq!((stopping.message.as_str(), stopping.confirm), ("Stop orders?", "Stop Anyway"));
        assert_eq!(
            stopping.detail,
            "3 staged changes in 2 tables have not been committed, and stay staged for when orders is \
             opened again. 1 change is held after a commit that got no answer, and stays held for when \
             orders is opened again."
        );
        assert!(!stopping.detail.contains("discards"));
        // Removing a remote forgets them with it.
        let removing = QuitRisks { staged: 1, tables: 1, ..Default::default() }.question_for(&remove).unwrap();
        assert_eq!(removing.message, "Discard 1 staged change and remove orders?");
        assert_eq!(removing.detail, "1 staged change has not been committed, and removing orders discards it.");
        assert_eq!(removing.confirm, "Discard and Remove");
        let removing = staged.question_for(&remove).unwrap();
        assert_eq!((removing.message.as_str(), removing.confirm), ("Remove orders?", "Remove Anyway"));
        assert!(removing.detail.ends_with("and removing orders drops the held copy."));

        // What leaving the connected database ends is asked about on every way out.
        let open = QuitRisks { transaction: true, running: true, ..Default::default() };
        let leaving = open.question_for(&switch).unwrap();
        assert_eq!((leaving.message.as_str(), leaving.confirm), ("Leave orders?", "Leave Anyway"));
        assert_eq!(
            leaving.detail,
            "The Query view holds a transaction open, and leaving orders rolls it back. A statement is \
             still running in the Query view. Leaving orders leaves its outcome unreported."
        );
        assert!(open.question_for(&stop).unwrap().detail.contains("and stopping orders rolls it back."));
        let committing = QuitRisks { committing: true, ..Default::default() };
        assert!(committing.question_for(&switch).unwrap().detail.contains("held for when orders is opened again"));
        assert!(committing.question_for(&remove).unwrap().detail.contains("are not kept here"));

        // Once nothing is at risk the dialog still names what goes ahead.
        let plain = stop.plain_question();
        assert_eq!(
            (plain.message.as_str(), plain.detail.as_str(), plain.confirm),
            ("Stop orders?", "Nothing is left that stopping orders would lose.", "Stop")
        );
        assert_eq!(Leaving::Quit.plain_question().detail, "Nothing is left that quitting would lose.");
    }

    #[test]
    fn an_upgrade_says_what_restarting_the_database_on_screen_ends() {
        use super::restart_notes;
        // Staged changes stay on screen through a restart: nothing to say.
        assert!(restart_notes("orders", &QuitRisks { staged: 3, tables: 1, ..Default::default() }).is_empty());
        let all = QuitRisks { transaction: true, running: true, committing: true, ..Default::default() };
        let notes = restart_notes("orders", &all);
        assert_eq!(notes.len(), 3);
        assert_eq!(notes[0], "The Query view holds a transaction open on orders, and the restart rolls it back.");
        assert!(notes[2].starts_with("A commit is still running on orders"));
    }

    #[test]
    fn a_connect_landing_asks_only_about_what_was_put_at_risk_while_it_ran() {
        use super::Leaving;
        let switch = Leaving::Switch { from: "a".into(), to: Aim::File("/tmp/b.duckdb".into()) };
        let nothing = QuitRisks::default();
        // Dialed with nothing at risk, it lands over a BEGIN typed since.
        let open = QuitRisks { transaction: true, ..Default::default() };
        let asked = open.since(&nothing).question_for(&switch).unwrap();
        assert_eq!((asked.message.as_str(), asked.confirm), ("Leave a?", "Leave Anyway"));
        // Left anyway over that transaction: the landing does not ask again.
        assert_eq!(open.since(&open).question_for(&switch), None);
        // But a statement started since is new.
        let running = QuitRisks { transaction: true, running: true, ..Default::default() };
        let asked = running.since(&open);
        assert!(!asked.transaction && asked.running);
        assert!(asked.question_for(&switch).is_some());
        // Staged sets are parked by a switch, and never a reason to ask.
        let staged = QuitRisks { staged: 4, tables: 1, held: 2, ..Default::default() };
        assert_eq!(staged.since(&nothing).question_for(&switch), None);
    }

    #[test]
    fn what_is_at_risk_depends_on_what_is_left() {
        use super::{Leaving, OnScreen, risks_of};
        use crate::edits::{Edits, Parked};
        let set = |table: &str, n: i64| {
            let mut e = Edits::new(
                format!("\"main\".\"{table}\""),
                vec!["id".into()],
                vec!["id".into()],
                vec!["INTEGER".into()],
            );
            for id in 0..n {
                e.stage_delete(vec![serde_json::json!(id)]);
            }
            e
        };
        let (a, b) = (DbKey::File("/data/a.duckdb".into()), DbKey::Remote("b".into()));
        let mut parked = Parked::default();
        parked.park(a.clone(), set("t", 2));
        parked.park(b.clone(), set("u", 3));
        let grid = set("v", 1);
        let screen = |committing: bool| OnScreen {
            sets: vec![&grid],
            committing,
            editing: true,
            transaction: true,
            running: false,
            exporting: true,
        };
        let quit = risks_of(&Leaving::Quit, Some(&a), Some(&a), screen(false), &parked);
        assert_eq!((quit.staged, quit.tables, quit.editing, quit.transaction), (6, 3, true, true));
        // An export holds a connection of its own: only the app's end
        // stops it, so leaving a database does not count it.
        assert!(quit.exporting);
        let relaunch = risks_of(&Leaving::Relaunch, Some(&a), Some(&a), screen(false), &parked);
        assert!(relaunch.exporting);
        // A commit in flight has the grid's set: it is not counted staged.
        let committing = risks_of(&Leaving::Quit, Some(&a), Some(&a), screen(true), &parked);
        assert_eq!((committing.staged, committing.tables, committing.committing), (5, 2, true));
        // Stopping the database on screen: its parked sets and the screen.
        let stop = Leaving::Stop { name: "a".into(), path: "/data/a.duckdb".into() };
        let here = risks_of(&stop, Some(&a), Some(&a), screen(false), &parked);
        assert_eq!((here.staged, here.tables, here.transaction), (3, 2, true));
        // Removing another: its parked sets alone, nothing on screen.
        let remove = Leaving::Remove { name: "b".into() };
        let there = risks_of(&remove, Some(&b), Some(&a), screen(false), &parked);
        assert_eq!(there, QuitRisks { staged: 3, tables: 1, ..Default::default() });
        // A switch with nothing connected risks nothing.
        let switch = Leaving::Switch { from: String::new(), to: Aim::File("/tmp/c.duckdb".into()) };
        assert_eq!(risks_of(&switch, None, None, screen(false), &parked), QuitRisks::default());
    }

    #[test]
    fn a_harbor_older_than_the_floor_is_refused_by_name() {
        use super::new_enough;
        for ok in ["0.44.2", "0.44.3", "0.45.0", "1.0.0", "v0.44.2", "0.44.2-dev"] {
            assert_eq!(new_enough(ok), Ok(()), "{ok}");
        }
        let old = new_enough("0.44.1").unwrap_err();
        assert!(old.starts_with("DuckTable needs Harbor 0.44.2 or later, and this server runs Harbor 0.44.1."), "{old}");
        assert!(new_enough("0.39.0").is_err());
        let unknown = new_enough("").unwrap_err();
        assert!(unknown.contains("this server does not say which Harbor it runs."), "{unknown}");
        assert!(new_enough("garbage").is_err());
    }

    /// Every kind of file opens in a scratch workspace through the functions
    /// the window calls: classified (`workspace::kind_of`), planned
    /// (`open::plan`), opened as a connect does (`connect` with
    /// `Aim::Workspace`), read back from the catalog the sidebar draws
    /// (`sidebar::tree_lines`), paged as the grid pages, a SQLite table
    /// edited through its attachment as ⌘S commits, a view imported as a
    /// table, removed; a SQLite file attached again by a restarted server
    /// from the workspace's record, detached and forgotten, and listed with
    /// why when it is gone at a start; and a spatial view read by a fresh
    /// server with nothing loaded by DuckTable.
    ///
    /// Live and scratch only: needs `HARBOR_HOME` set to a short scratch
    /// directory (the workspace is made there), `HARBOR_BIN` naming the
    /// harbor to run, and `DUCKTABLE_OPEN_PROBE` naming an empty scratch
    /// directory for the files; `sqlite3` makes the SQLite file.
    #[test]
    #[ignore]
    fn every_kind_of_file_opens_in_a_scratch_workspace() {
        use crate::sidebar::{tree_lines, Line};
        use harbor_client::workspace;
        use serde_json::json;
        let (Some(home), Some(dir)) = (std::env::var_os("HARBOR_HOME"), std::env::var_os("DUCKTABLE_OPEN_PROBE")) else {
            println!("set HARBOR_HOME and DUCKTABLE_OPEN_PROBE to scratch directories; skipping");
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        let ws = workspace::path().expect("workspace");
        assert!(ws.starts_with(&home), "the workspace must be the scratch one: {}", ws.display());
        let _ = std::fs::remove_file(&ws);
        let _ = std::fs::remove_file(ws.with_extension("duckdb.wal"));
        std::fs::create_dir_all(&dir).unwrap();

        // The files, made by the workspace's own engine where it can.
        let (conn, ..) = super::connect(Aim::Workspace(Vec::new())).expect("connect");
        let run = |sql: &str| harbor_client::exec(&conn, sql, None, None).unwrap_or_else(|e| panic!("{sql}: {e}"));
        let at = |name: &str| dir.join(name).to_str().unwrap().to_string();
        std::fs::write(dir.join("sales.csv"), "id,name,amount\n1,apple,1.5\n2,banana,2.25\n3,cherry,\n").unwrap();
        std::fs::write(
            dir.join("parks.geojson"),
            r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"name":"Central"},"geometry":{"type":"Point","coordinates":[-73.97,40.78]}}]}"#,
        )
        .unwrap();
        let _ = std::fs::remove_dir_all(dir.join("lake"));
        run(&format!(
            "COPY (SELECT range AS id, range % 2 + 2024 AS year FROM range(10)) TO '{}' (FORMAT parquet, PARTITION_BY (year))",
            at("lake")
        ));
        workspace::load_extension(&conn, "excel").expect("excel");
        run(&format!("COPY (SELECT range AS n, 'x' || range AS s FROM range(4)) TO '{}' (FORMAT xlsx, HEADER true)", at("book.xlsx")));
        let _ = std::fs::remove_file(dir.join("app.sqlite"));
        let made = std::process::Command::new("sqlite3")
            .arg(dir.join("app.sqlite"))
            .arg(
                "CREATE TABLE people(id INTEGER PRIMARY KEY, name TEXT NOT NULL, age INT); \
                 INSERT INTO people VALUES (1,'ann',30),(2,'bob',41),(3,'cy',22); \
                 CREATE TABLE notes(body TEXT, n INT); INSERT INTO notes VALUES ('hi',1),('yo',2);",
            )
            .status()
            .expect("sqlite3");
        assert!(made.success());
        std::fs::write(dir.join("notes.bin"), b"\x7fELF\x02\x01").unwrap();
        drop(conn);

        // Classified and planned as a drop of all of them is.
        let dropped = ["sales.csv", "lake", "book.xlsx", "app.sqlite", "parks.geojson", "notes.bin"].map(|n| dir.join(n));
        let plan = crate::open::plan(dropped.iter().map(|p| (p.clone(), workspace::kind_of(p))).collect());
        assert_eq!(plan.workspace.len(), 5);
        assert!(plan.refused[0].contains("notes.bin") && plan.refused[0].contains("7f 45 4c 46"), "{:?}", plan.refused);
        let (conn, _, catalog, opened) = super::connect(Aim::Workspace(plan.workspace.clone())).expect("open");
        let names: Vec<_> = opened.iter().map(|o| o.as_ref().map(|o| (o.name.as_str(), o.made)).unwrap()).collect();
        assert_eq!(names, [("sales", true), ("lake", true), ("book", true), ("app", true), ("parks", true)]);
        assert_eq!(crate::open::first_opened(&catalog, &opened), Some(Rel::own("main", "sales")));

        // The sidebar's tree: every view reads, spatial's among them, and
        // the SQLite file is a catalog of its own.
        let lines = tree_lines(&catalog, false, &None);
        for line in &lines {
            if let Line::View { rel, error, columns, file } = line {
                assert!(error.is_none() && *columns > 0 && *file, "{rel:?}: {error:?}");
            }
        }
        let rels: Vec<Rel> = lines.iter().filter_map(|l| l.rel().cloned()).collect();
        assert_eq!(
            rels,
            [
                Rel::own("main", "book"),
                Rel::own("main", "lake"),
                Rel::own("main", "parks"),
                Rel::own("main", "sales"),
                Rel::of("app", "main", "notes"),
                Rel::of("app", "main", "people"),
            ]
        );

        // Paged and sorted as the grid pages a view: read-only, no count,
        // every column breaking ties.
        let lake = crate::structure::structure_of(&catalog, &Rel::own("main", "lake")).unwrap();
        assert!(lake.view && crate::grid::identity_columns(Some(&lake)).is_empty());
        let columns: Vec<String> = lake.cols.iter().map(|c| c.name.clone()).collect();
        assert_eq!(columns, ["id", "year"]);
        let sort = vec![crate::sql::SortKey { column: "year".into(), desc: true }];
        let ties = crate::sql::tiebreak(&[], &columns);
        let (page, kept) = crate::sql::first_page(&conn, &Rel::own("main", "lake").source(), false, sort.clone(), &ties, 3);
        assert_eq!(kept, sort);
        assert_eq!(page.unwrap().rows, [[json!(1), json!(2025)], [json!(3), json!(2025)], [json!(5), json!(2025)]]);
        for rel in &rels {
            let page = crate::sql::first_page(&conn, &rel.source(), false, Vec::new(), &[], 500).0;
            assert!(page.is_ok_and(|p| !p.rows.is_empty()), "{rel:?}");
        }

        // A SQLite table edits through its attachment as ⌘S commits it:
        // keyed by its primary key, and a keyless one by rowid and hash.
        let commit = |e: &crate::edits::Edits| {
            let sid = harbor_client::session_new(&conn).expect("session");
            let outcome = crate::grid::run_statements(&conn, &sid, &e.statements())
                .and_then(|()| harbor_client::exec(&conn, "COMMIT", None, Some(&sid)).map(drop));
            harbor_client::session_release(&conn, &sid);
            outcome
        };
        let txt = |s: &str| Some(gpui_kit::SharedString::from(s.to_string()));
        let people = crate::structure::structure_of(&catalog, &Rel::of("app", "main", "people")).unwrap();
        assert_eq!(crate::grid::identity_columns(Some(&people)), ["id"]);
        // SQLite takes no INSERT … RETURNING (measured), so the grid's
        // set for it counts its INSERTs instead.
        assert!(people.foreign);
        let mut e = crate::edits::Edits::new(
            Rel::of("app", "main", "people").source(),
            vec!["id".into()],
            vec!["id".into(), "name".into(), "age".into()],
            vec!["BIGINT".into(), "VARCHAR".into(), "BIGINT".into()],
        )
        .without_returning();
        e.stage_cell(vec![json!(2)], 1, txt("bob"), txt("robert"), json!("robert"));
        e.stage_delete(vec![json!(3)]);
        let draft = e.stage_insert().unwrap();
        e.stage_insert_cell(&draft, 0, txt("9"), json!(9));
        e.stage_insert_cell(&draft, 1, txt("dee"), json!("dee"));
        e.stage_duplicate(vec![json!(1)], vec![(0, txt("10"), crate::edits::Bind::Value(json!(10))), (1, txt("ann"), crate::edits::Bind::Source)]);
        commit(&e).expect("the SQLite table takes the commit");
        let read = std::process::Command::new("sqlite3").arg(dir.join("app.sqlite")).arg("SELECT id, name FROM people ORDER BY id").output().unwrap();
        assert_eq!(String::from_utf8_lossy(&read.stdout), "1|ann\n2|robert\n9|dee\n10|ann\n");
        // A duplicate of a row that is gone inserts nothing, and is refused.
        let mut e = crate::edits::Edits::new(
            Rel::of("app", "main", "people").source(),
            vec!["id".into()],
            vec!["id".into(), "name".into(), "age".into()],
            vec!["BIGINT".into(), "VARCHAR".into(), "BIGINT".into()],
        )
        .without_returning();
        e.stage_duplicate(vec![json!(3)], vec![(1, txt("cy"), crate::edits::Bind::Source)]);
        assert!(commit(&e).unwrap_err().contains("source is gone"));
        let notes = crate::structure::structure_of(&catalog, &Rel::of("app", "main", "notes")).unwrap();
        assert!(notes.keyed_by_rowid());
        let source = Rel::of("app", "main", "notes").source();
        let page = harbor_client::query(&conn, &crate::sql::page_sql(&source, true, &None, 0, 10)).expect("page");
        let mut e = crate::edits::Edits::new(
            source,
            vec![],
            vec!["rowid".into(), "body".into(), "n".into()],
            vec!["UBIGINT[]".into(), "VARCHAR".into(), "BIGINT".into()],
        )
        .keyed_by_rowid();
        e.stage_cell(vec![page.rows[1][0].clone()], 1, txt("yo"), txt("hey"), json!("hey"));
        commit(&e).expect("the keyless SQLite table takes the commit");
        let read = std::process::Command::new("sqlite3").arg(dir.join("app.sqlite")).arg("SELECT body FROM notes ORDER BY n").output().unwrap();
        assert_eq!(String::from_utf8_lossy(&read.stdout), "hi\nhey\n");

        // Opened again, each file finds what the first opening made.
        let again = super::open_files(&conn, &plan.workspace);
        assert!(again.iter().all(|o| o.as_ref().is_ok_and(|o| !o.made && o.line.contains("already"))), "{again:?}");

        // Save As writes any table or view through Export's COPY.
        for (rel, file) in [(Rel::own("main", "parks"), "parks-out.csv"), (Rel::of("app", "main", "people"), "people-out.parquet")] {
            let path = dir.join(file);
            let format = crate::export::format_for(&path, crate::export::Format::Csv);
            let sql = crate::export::copy_sql(&format!("SELECT * FROM {}", rel.source()), path.to_str().unwrap(), crate::export::copy_options(format, &path));
            run_on(&conn, &sql);
            assert!(std::fs::metadata(&path).is_ok_and(|m| m.len() > 0), "{file}");
        }

        // Import as Table: the view becomes a table of its name, editable.
        assert_eq!(workspace::import(&conn, "main", "sales"), Ok(3));
        let catalog = super::read_catalog(&conn).expect("catalog");
        let sales = crate::structure::structure_of(&catalog, &Rel::own("main", "sales")).unwrap();
        assert!(!sales.view && sales.keyed_by_rowid());
        // The SQLite file is in the workspace's record from the moment it
        // is attached, before any server stops.
        let record = |conn: &harbor_client::Conn| {
            let sql = format!("SELECT name, path FROM {}.{} ORDER BY name", wire::open::RECORD_SCHEMA, wire::open::RECORD_TABLE);
            harbor_client::query(conn, &sql).expect("record").rows
        };
        assert_eq!(record(&conn), [[json!("app"), json!(at("app.sqlite"))]]);

        // A workspace server started again attaches the file again, and has
        // loaded `spatial` for the view over the GeoJSON before anyone asks.
        drop(conn);
        harbor_client::fleet::stop(&ws).expect("stop");
        let conn = harbor_client::fleet::connect_path(&ws).expect("connect again");
        let catalog = harbor_client::catalog(&conn).expect("catalog");
        assert!(workspace::needed_extensions(&catalog).is_empty());
        assert_eq!(catalog.views.iter().find(|v| v.name == "parks").map(|v| v.error.clone()), Some(None));
        let app: Vec<&str> = catalog.attached.iter().flat_map(|a| a.tables.iter().map(|t| t.name.as_str())).collect();
        assert_eq!(app, ["notes", "people"]);
        let people = "SELECT name FROM app.main.people ORDER BY id";
        assert_eq!(harbor_client::query(&conn, people).expect("people").rows.len(), 4);

        // Remove and Detach leave the files as they were; Detach lets go of
        // the record, so no server attaches the file again.
        workspace::remove_view(&conn, "main", "book").expect("remove");
        workspace::detach(&conn, "app", true).expect("detach");
        let catalog = super::read_catalog(&conn).expect("catalog");
        assert!(catalog.views.iter().all(|v| v.name != "book") && catalog.attached.is_empty());
        assert!(dir.join("book.xlsx").exists() && dir.join("app.sqlite").exists());
        assert!(record(&conn).is_empty());

        // A recorded file that is gone when the server starts is listed,
        // holding nothing, with why; the sidebar greys it, and its Detach,
        // whose DETACH IF EXISTS finds nothing, deletes the record's row.
        let reopened = super::open_files(&conn, &[(dir.join("app.sqlite"), wire::open::Kind::Sqlite)]);
        assert!(reopened[0].as_ref().is_ok_and(|o| o.name == "app" && o.made), "{reopened:?}");
        drop(conn);
        harbor_client::fleet::stop(&ws).expect("stop");
        std::fs::rename(dir.join("app.sqlite"), dir.join("app-away.sqlite")).unwrap();
        let conn = harbor_client::fleet::connect_path(&ws).expect("connect again");
        let catalog = super::read_catalog(&conn).expect("catalog");
        let gone = catalog.attached.iter().find(|a| a.name == "app").expect("the missing file is listed");
        let error = gone.error.clone().expect("with why");
        assert!(error.contains("no such file") && gone.tables.is_empty(), "{error}");
        let lines = tree_lines(&catalog, false, &None);
        assert!(lines.contains(&Line::Catalog { name: "app".into(), storage: "sqlite".into(), error: Some(error) }));
        assert!(!lines.iter().any(|l| l.rel().is_some_and(|r| r.catalog.as_deref() == Some("app"))));
        workspace::detach(&conn, "app", true).expect("detach the missing file");
        assert!(record(&conn).is_empty());
        assert!(super::read_catalog(&conn).expect("catalog").attached.is_empty());

        // Back where it was, the file stays unattached: the record let go.
        std::fs::rename(dir.join("app-away.sqlite"), dir.join("app.sqlite")).unwrap();
        drop(conn);
        harbor_client::fleet::stop(&ws).expect("stop");
        let conn = harbor_client::fleet::connect_path(&ws).expect("connect again");
        assert!(harbor_client::catalog(&conn).expect("catalog").attached.is_empty());
        drop(conn);
        harbor_client::fleet::stop(&ws).expect("stop");
    }

    fn run_on(conn: &harbor_client::Conn, sql: &str) {
        harbor_client::exec(conn, sql, None, None).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    #[test]
    fn catalog_refresh_accepts_only_the_current_connection_and_request() {
        assert!(catalog_refresh_is_current(7, 11, 7, 11));
        assert!(!catalog_refresh_is_current(8, 11, 7, 11));
        assert!(!catalog_refresh_is_current(7, 12, 7, 11));
    }
}
