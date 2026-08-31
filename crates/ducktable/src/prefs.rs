//! Global display preferences, persisted beside the theme choice.
//!
//! These are appearance settings, not table state: one value for the whole
//! app, saved to `~/.config/ducktable/prefs.json`, applied by whichever
//! surface cares (today: the grid).

use gpui::{App, Global};
use serde_json::{json, Value};

/// Which view of the table the footer has selected — a browsing mode,
/// not table state (it survives table switches), so it lives with the
/// other display prefs. Data and Structure are exclusive by design: a
/// schema change reshapes the data view, so the two never render side
/// by side.
#[derive(Clone, Copy, PartialEq)]
pub enum ViewMode {
    Data,
    Structure,
    Query,
}

#[derive(Clone, Copy)]
pub struct Prefs {
    pub row_numbers: bool,
    pub right_align: bool,
    pub null_tags: bool,
    pub view: ViewMode,
    /// The inspector pane's open state (UI.md: persists per window).
    pub inspector: bool,
    /// The inspector pane's width (UI.md: divider positions persist).
    pub inspector_width: f32,
    /// Rows per page in the data grid.
    pub page_size: usize,
    /// Index into [`ZOOMS`].
    pub zoom: usize,
    /// The sidebar's width (UI.md: divider positions persist).
    pub sidebar_width: f32,
    /// The window's last windowed frame (x, y, w, h) — None until the
    /// first resize or move, so a fresh install takes the platform's
    /// default placement.
    pub win: Option<(f32, f32, f32, f32)>,
}

impl Prefs {
    /// The current zoom's font multiplier for data-surface text.
    pub fn zoom_factor(&self) -> f32 {
        ZOOMS[self.zoom].0
    }

    /// The table chrome size matched to the current zoom.
    pub fn table_size(&self) -> gpui_component::Size {
        ZOOMS[self.zoom].1
    }
}

/// The page sizes the footer control cycles through: one decade apart, so
/// each step is a different kind of read (skim / work / bulk-scan).
pub const PAGE_SIZES: [usize; 3] = [500, 5_000, 50_000];

/// Zoom steps for the data surfaces (Cmd-= / Cmd-- / Cmd-0): a font
/// multiplier paired with the table chrome size whose row height fits it,
/// so zoomed text never clips its row. Chrome (sidebar, footer, labels)
/// stays put; the data is what zooms.
pub const ZOOMS: [(f32, gpui_component::Size); 6] = [
    (0.7, gpui_component::Size::XSmall),
    (0.85, gpui_component::Size::XSmall),
    (1.0, gpui_component::Size::XSmall),
    (1.15, gpui_component::Size::Small),
    (1.3, gpui_component::Size::Medium),
    (1.5, gpui_component::Size::Large),
];
pub const DEFAULT_ZOOM: usize = 2;

/// Inspector width bounds — the load clamp and the divider's drag range.
pub const INSPECTOR_MIN: f32 = 180.;
pub const INSPECTOR_MAX: f32 = 600.;

/// Sidebar width bounds — same double duty. The floor is the sidebar's
/// classic fixed width (Steve's ruling: today's size is the minimum;
/// the divider only ever grants more room).
pub const SIDEBAR_MIN: f32 = 224.;
pub const SIDEBAR_MAX: f32 = 480.;

impl Default for Prefs {
    fn default() -> Self {
        Self {
            row_numbers: true,
            right_align: false,
            null_tags: true,
            view: ViewMode::Data,
            inspector: false,
            inspector_width: 290.,
            page_size: 500,
            zoom: DEFAULT_ZOOM,
            sidebar_width: 224.,
            win: None,
        }
    }
}

impl Global for Prefs {}

fn file() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(std::path::Path::new(&home).join(".config").join("ducktable").join("prefs.json"))
}

pub fn init(cx: &mut App) {
    let mut prefs = Prefs::default();
    if let Some(v) = file()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
    {
        let read = |key: &str, or: bool| v.get(key).and_then(Value::as_bool).unwrap_or(or);
        prefs.row_numbers = read("row_numbers", prefs.row_numbers);
        prefs.right_align = read("right_align", prefs.right_align);
        prefs.null_tags = read("null_tags", prefs.null_tags);
        match v.get("view").and_then(Value::as_str) {
            Some("structure") => prefs.view = ViewMode::Structure,
            Some("query") => prefs.view = ViewMode::Query,
            _ => {}
        }
        prefs.inspector = read("inspector", prefs.inspector);
        prefs.inspector_width = v
            .get("inspector_width")
            .and_then(Value::as_f64)
            .map(|w| (w as f32).clamp(INSPECTOR_MIN, INSPECTOR_MAX))
            .unwrap_or(prefs.inspector_width);
        prefs.page_size = v
            .get("page_size")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .filter(|n| PAGE_SIZES.contains(n))
            .unwrap_or(prefs.page_size);
        prefs.zoom = v
            .get("zoom")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .filter(|n| *n < ZOOMS.len())
            .unwrap_or(prefs.zoom);
        prefs.sidebar_width = v
            .get("sidebar_width")
            .and_then(Value::as_f64)
            .map(|w| (w as f32).clamp(SIDEBAR_MIN, SIDEBAR_MAX))
            .unwrap_or(prefs.sidebar_width);
        // The window frame: four finite numbers with a plausible size,
        // or the platform default. (A frame saved on a display that no
        // longer exists still restores — macOS pulls windows on-screen.)
        prefs.win = v.get("window").and_then(Value::as_array).and_then(|a| {
            let n = |i: usize| a.get(i).and_then(Value::as_f64).map(|f| f as f32);
            match (n(0), n(1), n(2), n(3)) {
                (Some(x), Some(y), Some(w), Some(h)) if w >= 400. && h >= 300. => {
                    Some((x, y, w, h))
                }
                _ => None,
            }
        });
    }
    cx.set_global(prefs);
}

pub fn get(cx: &App) -> Prefs {
    *cx.global::<Prefs>()
}

/// Flip one preference, persist, and repaint.
pub fn toggle(cx: &mut App, change: impl FnOnce(&mut Prefs)) {
    save(cx, change);
    cx.refresh_windows();
}

/// Change and persist without a repaint — for values the UI already
/// reflects, like a divider position at the end of a drag.
pub fn save(cx: &mut App, change: impl FnOnce(&mut Prefs)) {
    let prefs = cx.global_mut::<Prefs>();
    change(prefs);
    let out = json!({
        "row_numbers": prefs.row_numbers,
        "right_align": prefs.right_align,
        "null_tags": prefs.null_tags,
        "view": match prefs.view {
            ViewMode::Data => "data",
            ViewMode::Structure => "structure",
            ViewMode::Query => "query",
        },
        "inspector": prefs.inspector,
        "inspector_width": prefs.inspector_width,
        "page_size": prefs.page_size,
        "zoom": prefs.zoom,
        "sidebar_width": prefs.sidebar_width,
        "window": prefs.win.map(|(x, y, w, h)| vec![x, y, w, h]),
    });
    if let Some(p) = file() {
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        std::fs::write(p, out.to_string()).ok();
    }
}
