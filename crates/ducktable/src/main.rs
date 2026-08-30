//! DuckTable: a fast, minimal desktop client for DuckDB, speaking to
//! DuckDB Harbor. This file is only the entry point; each surface owns a
//! file (`app.rs` state, `sidebar.rs`, `content.rs`, `theme.rs`).

mod app;
mod content;
mod copy_button;
mod footer;
mod grid;
mod inspector;
mod prefs;
mod queries;
mod sidebar;
mod structure;
mod theme;
mod util;

use app::DuckTable;
use gpui::*;
use gpui_component::{Root, StyledExt as _};

actions!(ducktable, [ToggleInspector, About, Quit]);

/// The macOS menu bar. The first menu becomes the application menu; the
/// About item opens the platform's standard dialog (window.prompt ->
/// NSAlert) with the version and a GitHub link.
fn app_menus() -> Vec<Menu> {
    vec![Menu {
        name: "DuckTable".into(),
        items: vec![
            MenuItem::action("About DuckTable", About),
            MenuItem::separator(),
            MenuItem::action("Quit DuckTable", Quit),
        ],
    }]
}

/// The About dialog: native, version-stamped, with a link out.
fn about(window: &mut Window, cx: &mut App) {
    let answer = window.prompt(
        PromptLevel::Info,
        concat!("DuckTable ", env!("CARGO_PKG_VERSION")),
        Some(
            "A fast, minimal desktop client for DuckDB, speaking to \
             DuckDB Harbor.\n\nMIT License \u{00b7} \u{00a9} 2026 Steve Shreeve",
        ),
        &["OK", "View on GitHub"],
        cx,
    );
    cx.spawn(async move |cx| {
        if answer.await == Ok(1) {
            cx.update(|cx| cx.open_url("https://github.com/shreeve/ducktable")).ok();
        }
    })
    .detach();
}

/// gpui-component's `IconName` resolves to `icons/*.svg` asset paths but
/// ships no files — the app serves them. Each icon used gets an embedded
/// entry here (Lucide, the set those names come from); a missing entry
/// renders as an invisible-but-clickable control.
struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        Ok(match path {
            "icons/panel-right.svg" => {
                Some(include_bytes!("../../../assets/icons/panel-right.svg").into())
            }
            "icons/search.svg" => {
                Some(include_bytes!("../../../assets/icons/search.svg").into())
            }
            "icons/refresh-cw.svg" => {
                Some(include_bytes!("../../../assets/icons/refresh-cw.svg").into())
            }
            "icons/chevron-left.svg" => {
                Some(include_bytes!("../../../assets/icons/chevron-left.svg").into())
            }
            "icons/chevron-right.svg" => {
                Some(include_bytes!("../../../assets/icons/chevron-right.svg").into())
            }
            "icons/chevron-first.svg" => {
                Some(include_bytes!("../../../assets/icons/chevron-first.svg").into())
            }
            "icons/chevron-last.svg" => {
                Some(include_bytes!("../../../assets/icons/chevron-last.svg").into())
            }
            "icons/eye.svg" => Some(include_bytes!("../../../assets/icons/eye.svg").into()),
            "icons/check.svg" => {
                Some(include_bytes!("../../../assets/icons/check.svg").into())
            }
            "icons/copy.svg" => {
                Some(include_bytes!("../../../assets/icons/copy.svg").into())
            }
            "icons/funnel.svg" => {
                Some(include_bytes!("../../../assets/icons/funnel.svg").into())
            }
            _ => None,
        })
    }

    fn list(&self, _: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}

impl Render for DuckTable {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .h_flex()
            .font_family(theme::ui_font())
            .on_action(cx.listener(|_, _: &ToggleInspector, _, cx| {
                prefs::toggle(cx, |p| p.inspector = !p.inspector);
            }))
            .child(self.sidebar(cx))
            .child(self.content(cx))
    }
}

fn main() {
    let app = Application::new().with_assets(Assets);

    app.run(move |cx| {
        gpui_component::init(cx);
        theme::init(cx);
        prefs::init(cx);
        cx.bind_keys([
            KeyBinding::new("cmd-alt-0", ToggleInspector, None),
            KeyBinding::new("cmd-q", Quit, None),
        ]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        // Global, not view-scoped: menu items must work regardless of
        // which pane holds focus. The prompt needs a window — but a menu
        // action arrives INSIDE the active window's update, so touching
        // that window again here is a re-entrant lease that fails
        // silently. Defer until the dispatch finishes.
        cx.on_action(|_: &About, cx| {
            cx.defer(|cx| {
                if let Some(w) = cx.active_window() {
                    w.update(cx, |_, window, cx| about(window, cx)).ok();
                }
            });
        });
        cx.set_menus(app_menus());

        cx.spawn(async move |cx| {
            cx.open_window(
                WindowOptions {
                    window_min_size: Some(size(px(720.), px(420.))),
                    ..Default::default()
                },
                |window, cx| {
                    let view = cx.new(DuckTable::new);
                    cx.new(|cx| Root::new(view, window, cx))
                },
            )?;

            Ok::<_, anyhow::Error>(())
        })
        .detach();
    });
}
