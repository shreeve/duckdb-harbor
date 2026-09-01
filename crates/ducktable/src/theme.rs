//! The one place DuckTable's semantic roles meet the active theme.
//!
//! Surfaces never name a color: they take a [`Pal`] at the top of a render
//! and read roles from it. The five themes live in
//! `assets/themes/ducktable.json` (embedded at build), transcribed from the
//! design tokens in `design/tokens.css`; switching themes re-derives every
//! surface because nothing else in the app holds a color.

// The data-surface type scale, from design/design.css `.grid`: 12px mono
// values, 600 11.5px UI headers, 11px muted row numbers, 10px tags and
// chips. Base sizes at zoom 1.0; the data surfaces multiply by the
// current zoom's factor (prefs::ZOOMS), whose paired table size keeps
// row heights ahead of the text. The gutter and chrome stay put.
/// The content pane's shared left inset: the title, the grid's first
/// column text, the Structure view, and the footer's view switcher all
/// start on this axis, so switching views never shifts the leftmost text.
pub const PANE_INSET: f32 = 12.;

pub const CELL_TEXT: f32 = 12.;
pub const HEADER_TEXT: f32 = 11.5;
pub const GUTTER_TEXT: f32 = 11.;
pub const TAG_TEXT: f32 = 10.;

use gpui::{App, Global, Hsla, SharedString};
use gpui_component::theme::{Theme, ThemeConfig, ThemeSet};
use gpui_component::ActiveTheme as _;
use harbor_client::Level;
use std::rc::Rc;

/// The roles a surface may paint with, resolved from the active theme.
#[derive(Clone, Copy)]
pub struct Pal {
    pub bg_sidebar: Hsla,
    pub surface: Hsla,
    pub raised: Hsla,
    /// One shade beyond `raised`, for a band that must read apart from an
    /// adjacent raised band (the grid title strip over the column
    /// headers). Derived, so it tracks every theme: darker than raised in
    /// light themes, lighter in dark ones.
    pub strip: Hsla,
    pub text: Hsla,
    pub muted: Hsla,
    pub accent: Hsla,
    /// Text painted ON a solid accent fill (tokens.css `--on-accent`).
    pub on_accent: Hsla,
    pub border: Hsla,
    pub good: Hsla,
    pub warn: Hsla,
    pub bad: Hsla,
    pub row_selected: Hsla,
    /// The grid's selected-row tint. Painted by the delegate's render_tr,
    /// UNDER the cell borders — the Table's own selection overlay is
    /// 1px-outset (top darker, bottom missing), so the themes zero
    /// `table.active.background` and the grid tints rows itself.
    pub row_active: Hsla,
    pub row_hover: Hsla,
    pub grid_line: Hsla,
}

impl gpui::Global for Pal {}

/// The derived palette. Cached as a global by `apply` — pal() runs per
/// cell per frame, so it must be a plain global read, not a re-derive.
pub fn pal(cx: &App) -> Pal {
    *cx.global::<Pal>()
}

fn compute_pal(cx: &App) -> Pal {
    let t = &cx.theme().colors;
    Pal {
        bg_sidebar: t.sidebar,
        surface: t.background,
        raised: t.secondary,
        strip: {
            let mut c = t.secondary;
            if t.background.l < 0.5 {
                c.l = (c.l + 0.045).min(1.);
            } else {
                c.l = (c.l - 0.045).max(0.);
            }
            c
        },
        text: t.foreground,
        muted: t.muted_foreground,
        accent: t.primary,
        on_accent: t.primary_foreground,
        border: t.border,
        good: t.success,
        warn: t.warning,
        bad: t.danger,
        row_selected: t.list_active,
        row_active: t.primary.opacity(if t.background.l < 0.5 { 0.17 } else { 0.10 }),
        row_hover: t.list_hover,
        grid_line: t.table_row_border,
    }
}

/// The one owner of the value font: every control that shows a stored
/// value uses this family (DESIGN.md: one font rule, one owner).
pub fn value_font() -> &'static str {
    if cfg!(target_os = "macos") {
        "Menlo"
    } else {
        "monospace"
    }
}

/// The UI font, for chrome inside value surfaces (e.g. the NULL tag).
pub fn ui_font() -> &'static str {
    ".SystemUIFont"
}

impl Pal {
    pub fn level(&self, level: Level) -> Hsla {
        match level {
            Level::Good => self.good,
            Level::Warn => self.warn,
            Level::Bad => self.bad,
            Level::Idle => self.muted,
        }
    }
}

struct Themes {
    configs: Vec<Rc<ThemeConfig>>,
    current: usize,
}

impl Global for Themes {}

fn choice_file() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(std::path::Path::new(&home).join(".config").join("ducktable").join("theme"))
}

/// Load the bundled themes, apply the persisted choice (or the set's
/// default for the system's light/dark mode), and install the cycler's
/// state. Call once at startup, after `gpui_component::init`.
pub fn init(cx: &mut App) {
    let set: ThemeSet =
        serde_json::from_str(include_str!("../../../assets/themes/ducktable.json"))
            .expect("bundled theme set parses");
    let configs: Vec<Rc<ThemeConfig>> = set.themes.into_iter().map(Rc::new).collect();

    let saved = choice_file().and_then(|p| std::fs::read_to_string(p).ok());
    let saved = saved.as_deref().map(str::trim);
    let current = configs
        .iter()
        .position(|c| Some(c.name.as_ref()) == saved)
        .unwrap_or_else(|| {
            let dark = cx.window_appearance() == gpui::WindowAppearance::Dark
                || cx.window_appearance() == gpui::WindowAppearance::VibrantDark;
            configs
                .iter()
                .position(|c| c.is_default && (c.mode.is_dark() == dark))
                .unwrap_or(0)
        });

    apply(&configs[current].clone(), cx);
    cx.set_global(Themes { configs, current });
}

fn apply(config: &Rc<ThemeConfig>, cx: &mut App) {
    let theme = Theme::global_mut(cx);
    if config.mode.is_dark() {
        theme.dark_theme = config.clone();
    } else {
        theme.light_theme = config.clone();
    }
    theme.mode = config.mode;
    theme.apply_config(config);
    cx.set_global(compute_pal(cx));
}

pub fn current_name(cx: &App) -> String {
    let themes = cx.global::<Themes>();
    themes.configs[themes.current].name.to_string()
}

/// The bundled themes in set order, each paired with whether it is dark.
/// The picker renders them grouped by mode, so the mode travels with the
/// name; the position in this list is the index [`select`] takes.
pub fn list(cx: &App) -> Vec<(SharedString, bool)> {
    cx.global::<Themes>()
        .configs
        .iter()
        .map(|c| (c.name.clone(), c.mode.is_dark()))
        .collect()
}

/// Which entry of [`list`] is active — the picker check-marks it.
pub fn current_index(cx: &App) -> usize {
    cx.global::<Themes>().current
}

/// Apply one theme by its index in [`list`], persist the choice, and
/// repaint. An out-of-range index, or the theme already showing, is a
/// quiet no-op: re-applying would repaint every surface for nothing.
pub fn select(ix: usize, cx: &mut App) {
    let config = {
        let themes = cx.global_mut::<Themes>();
        if ix >= themes.configs.len() || ix == themes.current {
            return;
        }
        themes.current = ix;
        themes.configs[ix].clone()
    };
    apply(&config, cx);
    persist(&config.name);
    cx.refresh_windows();
}

/// Write the chosen theme's name where `init` looks for it.
fn persist(name: &str) {
    if let Some(p) = choice_file() {
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        std::fs::write(p, name).ok();
    }
}
