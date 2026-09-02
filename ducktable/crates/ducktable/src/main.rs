//! DuckTable: a fast, minimal, and clean desktop client for DuckDB
//! Harbor. This file is only the entry point; each surface owns a
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
mod query;
mod sql;
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
        ToggleInspector, About, Quit, ZoomIn, ZoomOut, ZoomReset, FitColumns, TablePrev,
        TableNext, View1, View2, View3, ToggleFullScreen, ToggleRowNumbers, ToggleRightAlign,
        ToggleNullTags, OpenDatabase
    ]
);

/// Pick a theme by its index in `theme::list` — carried as data so one
/// action serves every theme the sidebar picker lists. `no_json`:
/// nothing loads DuckTable's keymap from disk, so the action needs no
/// serde derives.
#[derive(Clone, Default, PartialEq, Debug, Action)]
#[action(namespace = ducktable, no_json)]
pub struct SetTheme {
    pub ix: usize,
}

/// Right-click → Stop on a sidebar berth: shut its server down. Carries the
/// berth name as data, the way SetTheme carries its index, so one action
/// serves every row the sidebar lists.
#[derive(Clone, Default, PartialEq, Debug, Action)]
#[action(namespace = ducktable, no_json)]
pub struct StopBerth {
    pub name: String,
}

/// Right-click → Start a stopped berth. The rest of the lifecycle menu carries
/// the database file path as data (the verbs target the file, not the name).
#[derive(Clone, Default, PartialEq, Debug, Action)]
#[action(namespace = ducktable, no_json)]
pub struct StartBerth {
    pub path: String,
}

/// Right-click → Attach: add the berth to config.toml.
#[derive(Clone, Default, PartialEq, Debug, Action)]
#[action(namespace = ducktable, no_json)]
pub struct AttachBerth {
    pub path: String,
}

/// Right-click → Detach: remove the berth from config.toml.
#[derive(Clone, Default, PartialEq, Debug, Action)]
#[action(namespace = ducktable, no_json)]
pub struct DetachBerth {
    pub path: String,
}

/// Right-click → Autostart: the checkmark item. `on` carries the side to flip
/// to (the opposite of the current checkmark), so the toggle stays honest even
/// if the survey changed under the open menu.
#[derive(Clone, Default, PartialEq, Debug, Action)]
#[action(namespace = ducktable, no_json)]
pub struct ToggleAutostart {
    pub path: String,
    pub on: bool,
}

/// ⌥←/⌥→: the previous/next table in the sidebar (App::step_table).
/// App-level, like the view keys, so it works from any view; text
/// inputs stay safe — their own alt-arrow bindings (word jump) sit
/// deeper in the context stack and win while typing. The ⌥-arrow
/// grammar: ↑/↓ pages within a table, ←/→ circles the tables (with
/// rollover); views are ⌘1/⌘2/⌘3's job.
fn step_table(delta: i32, cx: &mut App) {
    let Some(view) = cx.try_global::<AppView>().and_then(|v| v.0.upgrade()) else {
        return;
    };
    cx.defer(move |cx| {
        if let Some(w) = cx.active_window() {
            w.update(cx, |_, window, cx| {
                view.update(cx, |this, cx| this.step_table(delta, window, cx));
            })
            .ok();
        }
    });
}

/// The switcher's one ordering — the footer renders it and ⌘1/⌘2/⌘3
/// address it (Finder's ⌘1–4 idiom; these keys migrate to the tab
/// strip when tabs ship, the ⌥↑/↓ pattern).
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
        Menu {
            name: "File".into(),
            items: vec![
                // The platform picker, then the same door a drop uses.
                // ⌘O advertises itself from the keymap binding.
                MenuItem::action("Open Database…", OpenDatabase),
            ],
        },
        // macOS shows each item's key equivalent from the keymap, so this
        // menu is also where the zoom shortcuts advertise themselves.
        Menu {
            name: "View".into(),
            items: vec![
                // macOS renders the ⌘1/⌘2/⌘3 and ⌥←/⌥→ key
                // equivalents from the keymap bindings.
                MenuItem::action("Structure", View1),
                MenuItem::action("Data", View2),
                MenuItem::action("Query", View3),
                MenuItem::separator(),
                MenuItem::action("Previous Table", TablePrev),
                MenuItem::action("Next Table", TableNext),
                MenuItem::separator(),
                // The header strip's toggles, together and in its own
                // order: the lozenge's three (⌥7/8/9 and ⌘7/8/9 both
                // fire; the menu shows one form — macOS allows a menu
                // item a single key equivalent), then the inspector
                // glyph beside them.
                MenuItem::action("Row Numbers", ToggleRowNumbers),
                MenuItem::action("Right-Align Numbers", ToggleRightAlign),
                MenuItem::action("NULL Tags", ToggleNullTags),
                MenuItem::action("Toggle Inspector", ToggleInspector),
                MenuItem::separator(),
                MenuItem::action("Zoom In", ZoomIn),
                MenuItem::action("Zoom Out", ZoomOut),
                MenuItem::action("Actual Size", ZoomReset),
                MenuItem::separator(),
                MenuItem::action("Fit Column Widths", FitColumns),
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
            "A fast, minimal, and clean desktop client for DuckDB \
             Harbor.\n\nMIT License \u{00a9} 2026 Steve Shreeve",
        ),
        &["OK", "View on GitHub"],
        cx,
    );
    cx.spawn(async move |cx| {
        if answer.await == Ok(1) {
            cx.update(|cx| cx.open_url("https://github.com/shreeve/duckdb-harbor")).ok();
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

const ICONS: [(&str, &[u8]); 12] = [
    icon!("shapes"),
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
            // Drop a database file anywhere on the window: the same door
            // as File→Open. First path wins — a multi-file drop opening N
            // databases would be N-1 surprises.
            .on_drop(cx.listener(|this, dropped: &ExternalPaths, _, cx| {
                if let Some(path) = dropped.paths().first().cloned() {
                    this.open_path(path, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &FitColumns, _, cx| {
                if let Some(grid) = &this.grid {
                    grid.update(cx, |grid, cx| grid.fit_columns(cx));
                }
            }))
            // The sidebar/content divider drags rightward from the
            // classic fixed width — today's size is the floor, the
            // divider only grants more room — and persists like the
            // inspector's does.
            .child(
                gpui_component::resizable::h_resizable("root-split")
                    .with_state(&self.sidebar_resize)
                    .child(
                        gpui_component::resizable::resizable_panel()
                            .size(px(prefs::get(cx).sidebar_width))
                            .size_range(px(prefs::SIDEBAR_MIN)..px(prefs::SIDEBAR_MAX))
                            // Furniture: only the user's drag changes
                            // this width — a window resize gives all
                            // its delta to the content.
                            .fixed()
                            .child(self.sidebar(cx)),
                    )
                    .child(
                        gpui_component::resizable::resizable_panel()
                            .child(self.content(cx)),
                    ),
            )
    }
}

/// macOS auto-appends "Enter Full Screen" (icon and all) to any menu
/// named View, which also forces an icon column that indents its
/// neighbors. This AppKit default, registered before the menu is built,
/// turns the injection off; the green traffic light still fullscreens.
#[cfg(target_os = "macos")]
// The objc crate's macros probe cfg(cargo-clippy), which rustc now
// flags; the allow keeps OUR build warning-clean without touching the
// vendored macro.
#[allow(unexpected_cfgs)]
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
            KeyBinding::new("cmd-i", ToggleInspector, None),
            KeyBinding::new("cmd-o", OpenDatabase, None),
            KeyBinding::new("cmd-q", Quit, None),
            // Cmd-Plus arrives as cmd-= (unshifted) or cmd-shift-= — bind
            // both, the way browsers treat the pair.
            KeyBinding::new("cmd-=", ZoomIn, None),
            KeyBinding::new("cmd-shift-=", ZoomIn, None),
            KeyBinding::new("cmd--", ZoomOut, None),
            KeyBinding::new("cmd-0", ZoomReset, None),
            KeyBinding::new("cmd-shift-f", FitColumns, None),
            KeyBinding::new("alt-left", TablePrev, None),
            KeyBinding::new("alt-right", TableNext, None),
            KeyBinding::new("ctrl-cmd-f", ToggleFullScreen, None),
            KeyBinding::new("cmd-1", View1, None),
            KeyBinding::new("cmd-2", View2, None),
            KeyBinding::new("cmd-3", View3, None),
            // Twin shortcuts: ⌥ digits and ⌘ digits (muscle memory
            // beside ⌘1/2/3) both fire the toggles. The menu can only
            // advertise one — macOS gives a menu item a single key
            // equivalent — so it shows the ⌘ form. Known trade: on
            // layouts that TYPE with Option (German ⌥7 = |), the ⌥
            // bindings shadow those characters in text inputs.
            KeyBinding::new("alt-7", ToggleRowNumbers, None),
            KeyBinding::new("alt-8", ToggleRightAlign, None),
            KeyBinding::new("alt-9", ToggleNullTags, None),
            KeyBinding::new("cmd-7", ToggleRowNumbers, None),
            KeyBinding::new("cmd-8", ToggleRightAlign, None),
            KeyBinding::new("cmd-9", ToggleNullTags, None),
        ]);
        // File→Open: the platform picker, then app.open_path — the same
        // door a drag-drop uses. .duckdb is what it speaks today; the
        // open-anything dispatcher (CSV, Parquet, Sheets URLs…) grows on
        // this trunk. The picker offers no extension filter (gpui's
        // PathPromptOptions has none), and none is enforced here: a wrong
        // file fails honestly in the connect card with harbor's own error.
        cx.on_action(|_: &OpenDatabase, cx| {
            let rx = cx.prompt_for_paths(PathPromptOptions {
                files: true,
                directories: false,
                multiple: false,
                prompt: Some("Open".into()),
            });
            cx.spawn(async move |cx| {
                if let Ok(Ok(Some(mut paths))) = rx.await
                    && let Some(path) = paths.pop()
                {
                    cx.update(|cx| {
                        if let Some(view) =
                            cx.try_global::<AppView>().and_then(|v| v.0.upgrade())
                        {
                            view.update(cx, |this, cx| this.open_path(path, cx));
                        }
                    })
                    .ok();
                }
            })
            .detach();
        });
        // Right-click → Stop. Reaches the view through AppView (the menu
        // fires at App level) and defers, the FitColumns pattern — a menu
        // action arrives inside the window's update, so the view is touched
        // on the next tick, not re-entrantly.
        cx.on_action(|a: &StopBerth, cx| {
            let name = a.name.clone();
            if let Some(view) = cx.try_global::<AppView>().and_then(|v| v.0.upgrade()) {
                cx.defer(move |cx| {
                    view.update(cx, |this, cx| this.stop_berth(name, cx));
                });
            }
        });
        cx.on_action(|a: &StartBerth, cx| {
            let path = std::path::PathBuf::from(&a.path);
            if let Some(view) = cx.try_global::<AppView>().and_then(|v| v.0.upgrade()) {
                cx.defer(move |cx| {
                    view.update(cx, |this, cx| this.start_berth(path, cx));
                });
            }
        });
        cx.on_action(|a: &AttachBerth, cx| {
            let path = std::path::PathBuf::from(&a.path);
            if let Some(view) = cx.try_global::<AppView>().and_then(|v| v.0.upgrade()) {
                cx.defer(move |cx| {
                    view.update(cx, |this, cx| this.attach_berth(path, cx));
                });
            }
        });
        cx.on_action(|a: &DetachBerth, cx| {
            let path = std::path::PathBuf::from(&a.path);
            if let Some(view) = cx.try_global::<AppView>().and_then(|v| v.0.upgrade()) {
                cx.defer(move |cx| {
                    view.update(cx, |this, cx| this.detach_berth(path, cx));
                });
            }
        });
        cx.on_action(|a: &ToggleAutostart, cx| {
            let path = std::path::PathBuf::from(&a.path);
            let on = a.on;
            if let Some(view) = cx.try_global::<AppView>().and_then(|v| v.0.upgrade()) {
                cx.defer(move |cx| {
                    view.update(cx, |this, cx| this.toggle_autostart(path, on, cx));
                });
            }
        });
        cx.on_action(|_: &TablePrev, cx| step_table(-1, cx));
        cx.on_action(|_: &TableNext, cx| step_table(1, cx));
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
        // The sidebar picker dispatches this into the window; it lands
        // here, the way the zoom and view actions do.
        cx.on_action(|a: &SetTheme, cx| theme::select(a.ix, cx));
        // Global fallback so the menu item validates and works; the
        // view-scoped listener handles the keyboard path first and a
        // handled action never reaches here (no double toggle).
        cx.on_action(|_: &ToggleInspector, cx| {
            prefs::toggle(cx, |p| p.inspector = !p.inspector);
        });
        // The lozenge's display toggles (⌘7/⌘8/⌘9): global prefs, so
        // the keyboard path is exactly the click path — every grid
        // self-heals from prefs at the top of its own render.
        cx.on_action(|_: &ToggleRowNumbers, cx| {
            prefs::toggle(cx, |p| p.row_numbers = !p.row_numbers);
        });
        cx.on_action(|_: &ToggleRightAlign, cx| {
            prefs::toggle(cx, |p| p.right_align = !p.right_align);
        });
        cx.on_action(|_: &ToggleNullTags, cx| {
            prefs::toggle(cx, |p| p.null_tags = !p.null_tags);
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

        // The window opens where it last stood — the frame saved on
        // every move and resize below. A fresh install has no frame
        // and takes the platform default.
        let remembered = prefs::get(cx).win.map(|(x, y, w, h)| {
            WindowBounds::Windowed(Bounds::new(
                point(px(x), px(y)),
                size(px(w), px(h)),
            ))
        });
        cx.spawn(async move |cx| {
            cx.open_window(
                WindowOptions {
                    window_min_size: Some(size(px(720.), px(420.))),
                    window_bounds: remembered,
                    ..Default::default()
                },
                |window, cx| {
                    let view = cx.new(DuckTable::new);
                    // The one window's view, reachable from App-level
                    // action handlers (FitColumns above).
                    cx.set_global(AppView(view.downgrade()));
                    view.update(cx, |_, cx| {
                        // Fires on move and resize both; fullscreen
                        // frames are the display's, not the user's, so
                        // they don't overwrite the remembered one. The
                        // SIZE saved is the content's (viewport), not
                        // the outer frame's: macOS restores through
                        // initWithContentRect, so an outer-frame size
                        // would regrow by one titlebar every launch.
                        cx.observe_window_bounds(window, |_, window, cx| {
                            if window.is_fullscreen() {
                                return;
                            }
                            let origin = window.bounds().origin;
                            let content = window.viewport_size();
                            prefs::save(cx, |p| {
                                p.win = Some((
                                    f32::from(origin.x),
                                    f32::from(origin.y),
                                    f32::from(content.width),
                                    f32::from(content.height),
                                ));
                            });
                        })
                        .detach();
                    });
                    cx.new(|cx| Root::new(view, window, cx))
                },
            )?;

            Ok::<_, anyhow::Error>(())
        })
        .detach();
    });
}
