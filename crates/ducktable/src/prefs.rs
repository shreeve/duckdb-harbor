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
}

/// The page sizes the footer control cycles through.
pub const PAGE_SIZES: [usize; 4] = [500, 1_000, 5_000, 10_000];

impl Default for Prefs {
    fn default() -> Self {
        Self {
            row_numbers: true,
            right_align: false,
            null_tags: true,
            inspector: false,
            inspector_width: 290.,
            page_size: 5_000,
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
            .map(|w| (w as f32).clamp(180., 600.))
            .unwrap_or(prefs.inspector_width);
        prefs.page_size = v
            .get("page_size")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .filter(|n| PAGE_SIZES.contains(n))
            .unwrap_or(prefs.page_size);
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
    });
    if let Some(p) = file() {
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        std::fs::write(p, out.to_string()).ok();
    }
}
