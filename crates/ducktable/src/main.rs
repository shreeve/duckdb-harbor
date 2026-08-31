//! DuckTable: a fast, minimal desktop client for DuckDB, speaking to
//! DuckDB Harbor. This file is only the entry point; each surface owns a
//! file (`app.rs` state, `sidebar.rs`, `content.rs`, `theme.rs`).

mod app;
mod chrome;
mod content;
mod copy_button;
mod edits;
mod footer;
mod grid;
mod inspector;
mod prefs;
mod queries;
mod query;
mod sidebar;
mod structure;
mod theme;
mod util;

use app::DuckTable;
use gpui::*;
use gpui_component::{Root, StyledExt as _};

actions!(
    ducktable,
    [
        ToggleInspector, About, Quit, ZoomIn, ZoomOut, ZoomReset, FitColumns, ViewPrev,
        ViewNext, View1, View2, View3, ToggleFullScreen
    ]
);

/// ⌥←/⌥→: walk the footer's view switcher with rollover — a carousel,
/// not a wall. App-level, unlike the grid's keymap, because it must
/// keep working in Structure mode, where the table (and the focus that
/// feeds the grid's listeners) isn't even rendered. Text inputs stay
/// safe: their own alt-arrow bindings (word jump) sit deeper in the
/// context stack and win while typing.
fn step_view(dir: i32, cx: &mut App) {
    let modes = view_order();
    let cur = prefs::get(cx).view;
    let ix = modes.iter().position(|v| *v == cur).unwrap_or(0) as i32;
    go_view(modes[(ix + dir).rem_euclid(modes.len() as i32) as usize], cx);
}

/// The switcher's one ordering — the footer renders it, the carousel
/// walks it, and ⌘1/⌘2/⌘3 address it (Finder's ⌘1–4 idiom; these keys
/// migrate to the tab strip when tabs ship, the ⌥↑/↓ pattern).
fn view_order() -> [prefs::ViewMode; 3] {
    use prefs::ViewMode;
    [ViewMode::Structure, ViewMode::Data, ViewMode::Query]
}

/// Land on a view: select it and hand focus to its surface.
fn go_view(next: prefs::ViewMode, cx: &mut App) {
    use prefs::ViewMode;
    prefs::toggle(cx, |p| p.view = next);
    // Landing on Data hands focus back to the table; landing on Query
    // hands it to the editor — the same symmetry (docs/QUERY.md).
    if matches!(next, ViewMode::Data | ViewMode::Query) {
        if let Some(view) = cx.try_global::<AppView>().and_then(|v| v.0.upgrade()) {
            cx.defer(move |cx| {
                view.update(cx, |this, cx| {
                    match next {
                        ViewMode::Data => {
                            if let Some(grid) = &this.grid {
                                grid.update(cx, |grid, cx| grid.request_focus(cx));
                            }
                        }
                        ViewMode::Query => this.focus_query(cx),
                        ViewMode::Structure => {}
                    }
                });
            });
        }
    }
}

/// The one window's root view, for App-level action handlers that need
/// to reach into it (menus fire at App level when focus skips the view).
struct AppView(WeakEntity<DuckTable>);

impl Global for AppView {}

/// The macOS menu bar. The first menu becomes the application menu; the
/// About item opens the platform's standard dialog (window.prompt ->
/// NSAlert) with the version and a GitHub link.
fn app_menus() -> Vec<Menu> {
    vec![
        Menu {
            name: "DuckTable".into(),
            items: vec![
                MenuItem::action("About DuckTable", About),
                MenuItem::separator(),
                MenuItem::action("Quit DuckTable", Quit),
            ],
        },
        // macOS shows each item's key equivalent from the keymap, so this
        // menu is also where the zoom shortcuts advertise themselves.
        Menu {
            name: "View".into(),
            items: vec![
                // The carousel advertises itself here: macOS renders the
                // ⌥←/⌥→ key equivalents from the keymap bindings.
                MenuItem::action("Structure", View1),
                MenuItem::action("Data", View2),
                MenuItem::action("Query", View3),
                MenuItem::separator(),
                MenuItem::action("Previous View", ViewPrev),
                MenuItem::action("Next View", ViewNext),
                MenuItem::separator(),
                MenuItem::action("Zoom In", ZoomIn),
                MenuItem::action("Zoom Out", ZoomOut),
                MenuItem::action("Actual Size", ZoomReset),
                MenuItem::separator(),
                MenuItem::action("Fit Column Widths", FitColumns),
                MenuItem::action("Toggle Inspector", ToggleInspector),
                MenuItem::separator(),
                // Ours, not AppKit's injected one (suppressed above for
                // its icon and forced indent) — plain text, same slot.
                MenuItem::action("Toggle Full Screen", ToggleFullScreen),
            ],
        },
    ]
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

macro_rules! icon {
    ($name:literal) => {
        (concat!("icons/", $name, ".svg"), include_bytes!(concat!("../../../assets/icons/", $name, ".svg")) as &[u8])
    };
}

const ICONS: [(&str, &[u8]); 11] = [
    icon!("panel-right"),
    icon!("search"),
    icon!("refresh-cw"),
    icon!("chevron-left"),
    icon!("chevron-right"),
    icon!("chevron-first"),
    icon!("chevron-last"),
    icon!("eye"),
    icon!("check"),
    icon!("copy"),
    icon!("funnel"),
];

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        Ok(ICONS.iter().find(|(p, _)| *p == path).map(|(_, bytes)| (*bytes).into()))
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
            .on_action(cx.listener(|this, _: &FitColumns, _, cx| {
                if let Some(grid) = &this.grid {
                    grid.update(cx, |grid, cx| grid.fit_columns(cx));
                }
            }))
            .child(self.sidebar(cx))
            .child(self.content(cx))
    }
}

/// macOS auto-appends "Enter Full Screen" (icon and all) to any menu
/// named View, which also forces an icon column that indents its
/// neighbors. This AppKit default, registered before the menu is built,
/// turns the injection off; the green traffic light still fullscreens.
#[cfg(target_os = "macos")]
fn suppress_fullscreen_menu_item() {
    use objc::runtime::{Object, NO};
    use objc::{class, msg_send, sel, sel_impl};
    unsafe {
        let key: *mut Object = msg_send![
            class!(NSString),
            stringWithUTF8String: c"NSFullScreenMenuItemEverywhere".as_ptr()
        ];
        let no: *mut Object = msg_send![class!(NSNumber), numberWithBool: NO];
        let dict: *mut Object =
            msg_send![class!(NSDictionary), dictionaryWithObject: no forKey: key];
        let defaults: *mut Object = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let _: () = msg_send![defaults, registerDefaults: dict];
    }
}

fn main() {
    #[cfg(target_os = "macos")]
    suppress_fullscreen_menu_item();
    let app = Application::new().with_assets(Assets);

    app.run(move |cx| {
        gpui_component::init(cx);
        // The DuckDB grammar (crates/duckdb-lang), registered before any
        // editor renders: the Query view asks for language "duckdb".
        gpui_component::highlighter::LanguageRegistry::singleton().register(
            "duckdb",
            &gpui_component::highlighter::LanguageConfig::new(
                "duckdb",
                duckdb_lang::LANGUAGE.into(),
                vec![],
                duckdb_lang::HIGHLIGHTS,
                duckdb_lang::INJECTIONS,
                duckdb_lang::LOCALS,
            ),
        );
        theme::init(cx);
        prefs::init(cx);
        cx.bind_keys([
            KeyBinding::new("cmd-alt-0", ToggleInspector, None),
            KeyBinding::new("cmd-q", Quit, None),
            // Cmd-Plus arrives as cmd-= (unshifted) or cmd-shift-= — bind
            // both, the way browsers treat the pair.
            KeyBinding::new("cmd-=", ZoomIn, None),
            KeyBinding::new("cmd-shift-=", ZoomIn, None),
            KeyBinding::new("cmd--", ZoomOut, None),
            KeyBinding::new("cmd-0", ZoomReset, None),
            KeyBinding::new("cmd-shift-f", FitColumns, None),
            KeyBinding::new("alt-left", ViewPrev, None),
            KeyBinding::new("alt-right", ViewNext, None),
            KeyBinding::new("ctrl-cmd-f", ToggleFullScreen, None),
            KeyBinding::new("cmd-1", View1, None),
            KeyBinding::new("cmd-2", View2, None),
            KeyBinding::new("cmd-3", View3, None),
        ]);
        cx.on_action(|_: &ViewPrev, cx| step_view(-1, cx));
        cx.on_action(|_: &ViewNext, cx| step_view(1, cx));
        cx.on_action(|_: &ToggleFullScreen, cx| {
            cx.defer(|cx| {
                if let Some(w) = cx.active_window() {
                    w.update(cx, |_, window, _| window.toggle_fullscreen()).ok();
                }
            });
        });
        cx.on_action(|_: &View1, cx| go_view(view_order()[0], cx));
        cx.on_action(|_: &View2, cx| go_view(view_order()[1], cx));
        cx.on_action(|_: &View3, cx| go_view(view_order()[2], cx));
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_action(|_: &ZoomIn, cx| {
            prefs::toggle(cx, |p| p.zoom = (p.zoom + 1).min(prefs::ZOOMS.len() - 1));
        });
        cx.on_action(|_: &ZoomOut, cx| {
            prefs::toggle(cx, |p| p.zoom = p.zoom.saturating_sub(1));
        });
        cx.on_action(|_: &ZoomReset, cx| {
            prefs::toggle(cx, |p| p.zoom = prefs::DEFAULT_ZOOM);
        });
        // Global fallback so the menu item validates and works; the
        // view-scoped listener handles the keyboard path first and a
        // handled action never reaches here (no double toggle).
        cx.on_action(|_: &ToggleInspector, cx| {
            prefs::toggle(cx, |p| p.inspector = !p.inspector);
        });
        // FitColumns needs the window's grid, which this handler reaches
        // through the app-view handle — directly, never by re-dispatching
        // into the window. A re-dispatch here once looped forever: with
        // focus in a text input the window listener is off the dispatch
        // path, so the action came straight back to this handler, which
        // deferred it again, and Cmd-Shift-F pinned the main thread until
        // force-quit. This handler both validates the menu item and does
        // the work when the view-scoped listener was skipped (a handled
        // dispatch never reaches here, so there is no double fit).
        cx.on_action(|_: &FitColumns, cx| {
            let Some(view) = cx.try_global::<AppView>().and_then(|v| v.0.upgrade()) else {
                return;
            };
            cx.defer(move |cx| {
                view.update(cx, |this, cx| {
                    if let Some(grid) = &this.grid {
                        grid.update(cx, |grid, cx| grid.fit_columns(cx));
                    }
                });
            });
        });
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
                    // The one window's view, reachable from App-level
                    // action handlers (FitColumns above).
                    cx.set_global(AppView(view.downgrade()));
                    cx.new(|cx| Root::new(view, window, cx))
                },
            )?;

            Ok::<_, anyhow::Error>(())
        })
        .detach();
    });
}
