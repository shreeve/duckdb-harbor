//! Global display preferences, persisted beside the theme choice.
//!
//! These are appearance settings, not table state: one value for the whole
//! app, saved to `~/.config/ducktable/prefs.json`, applied by whichever
//! surface cares (today: the grid).

use gpui::{App, Global};
use serde_json::{json, Value};

#[derive(Clone, Copy)]
pub struct Prefs {
    pub row_numbers: bool,
    pub right_align: bool,
    pub null_tags: bool,
    /// The inspector pane's open state (UI.md: persists per window).
    pub inspector: bool,
    /// The inspector pane's width (UI.md: divider positions persist).
    pub inspector_width: f32,
    /// Rows per page in the data grid.
    pub page_size: usize,
    /// Index into [`ZOOMS`].
    pub zoom: usize,
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

impl Default for Prefs {
    fn default() -> Self {
        Self {
            row_numbers: true,
            right_align: false,
            null_tags: true,
            inspector: false,
            inspector_width: 290.,
            page_size: 500,
            zoom: DEFAULT_ZOOM,
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
        "inspector": prefs.inspector,
        "inspector_width": prefs.inspector_width,
        "page_size": prefs.page_size,
        "zoom": prefs.zoom,
    });
    if let Some(p) = file() {
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        std::fs::write(p, out.to_string()).ok();
    }
}
