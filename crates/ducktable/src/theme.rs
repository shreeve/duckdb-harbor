//! The one place DuckTable's semantic roles meet the active theme.
//!
//! Surfaces never name a color: they take a [`Pal`] at the top of a render
//! and read roles from it. The five themes live in
//! `assets/themes/ducktable.json` (embedded at build), transcribed from the
//! design tokens in `design/tokens.css`; switching themes re-derives every
//! surface because nothing else in the app holds a color.

use gpui::{App, Global, Hsla};
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
    pub text: Hsla,
    pub muted: Hsla,
    pub accent: Hsla,
    pub border: Hsla,
    pub good: Hsla,
    pub warn: Hsla,
    pub bad: Hsla,
    pub row_selected: Hsla,
    pub row_hover: Hsla,
}

pub fn pal(cx: &App) -> Pal {
    let t = &cx.theme().colors;
    Pal {
        bg_sidebar: t.sidebar,
        surface: t.background,
        raised: t.secondary,
        text: t.foreground,
        muted: t.muted_foreground,
        accent: t.primary,
        border: t.border,
        good: t.success,
        warn: t.warning,
        bad: t.danger,
        row_selected: t.list_active,
        row_hover: t.list_hover,
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
}

pub fn current_name(cx: &App) -> String {
    let themes = cx.global::<Themes>();
    themes.configs[themes.current].name.to_string()
}

/// Advance to the next theme, apply it, persist the choice, and repaint.
pub fn cycle(cx: &mut App) {
    let (config, name) = {
        let themes = cx.global_mut::<Themes>();
        themes.current = (themes.current + 1) % themes.configs.len();
        let c = themes.configs[themes.current].clone();
        let name = c.name.to_string();
        (c, name)
    };
    apply(&config, cx);
    if let Some(p) = choice_file() {
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        std::fs::write(p, name).ok();
    }
    cx.refresh_windows();
}
