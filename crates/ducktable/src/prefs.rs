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
}

impl Default for Prefs {
    fn default() -> Self {
        Self { row_numbers: true, right_align: false, null_tags: true }
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
    }
    cx.set_global(prefs);
}

pub fn get(cx: &App) -> Prefs {
    *cx.global::<Prefs>()
}

/// Flip one preference, persist, and repaint.
pub fn toggle(cx: &mut App, change: impl FnOnce(&mut Prefs)) {
    let prefs = cx.global_mut::<Prefs>();
    change(prefs);
    let out = json!({
        "row_numbers": prefs.row_numbers,
        "right_align": prefs.right_align,
        "null_tags": prefs.null_tags,
    });
    if let Some(p) = file() {
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        std::fs::write(p, out.to_string()).ok();
    }
    cx.refresh_windows();
}
