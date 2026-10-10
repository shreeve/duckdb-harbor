//! DuckTable: a fast, minimal, and clean desktop client for DuckDB
//! Harbor. This file is only the entry point; each surface owns a
//! file (`app.rs` state, `sidebar.rs`, `content.rs`, `theme.rs`).

mod app;
mod add_database;
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
mod updater;
mod util;

use app::DuckTable;
use gpui_kit::*;
use gpui_kit::component::StyledExt as _;

actions!(
    ducktable,
    [
        ToggleInspector, About, Quit, ZoomIn, ZoomOut, ZoomReset, FitColumns, RefreshTables,
        AddRow, DuplicateRow, DeleteRow, TablePrev, TableNext, View1, View2, View3, ToggleFullScreen,
        ToggleRowNumbers, ToggleRightAlign, ToggleNullTags, ToggleColumnCards, OpenDatabase,
        OpenDatabaseUrl,
        CheckForUpdates
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
/// row's database file as data, the way SetTheme carries its index, so one
/// action serves every row the sidebar lists. The file is what is stopped;
/// the name is only what a failure is reported under, since two databases
/// can share one. The path is carried as the path it is, like every action
/// below: a file name need not be UTF-8.
#[derive(Clone, Default, PartialEq, Debug, Action)]
#[action(namespace = ducktable, no_json)]
pub struct StopBerth {
    pub name: String,
    pub path: std::path::PathBuf,
}

/// Right-click → Start a stopped berth. The rest of the lifecycle menu carries
/// the database file path as data (the verbs target the file, not the name).
#[derive(Clone, Default, PartialEq, Debug, Action)]
#[action(namespace = ducktable, no_json)]
pub struct StartBerth {
    pub path: std::path::PathBuf,
}

/// Right-click → Attach: add the berth to config.toml.
#[derive(Clone, Default, PartialEq, Debug, Action)]
#[action(namespace = ducktable, no_json)]
pub struct AttachBerth {
    pub path: std::path::PathBuf,
}

/// Right-click → Detach: remove the berth from config.toml.
#[derive(Clone, Default, PartialEq, Debug, Action)]
#[action(namespace = ducktable, no_json)]
pub struct DetachBerth {
    pub path: std::path::PathBuf,
}

/// Right-click → Autostart: the checkmark item. `on` carries the side to flip
/// to (the opposite of the current checkmark), so the toggle stays honest even
/// if the survey changed under the open menu.
#[derive(Clone, Default, PartialEq, Debug, Action)]
#[action(namespace = ducktable, no_json)]
pub struct ToggleAutostart {
    pub path: std::path::PathBuf,
    pub on: bool,
}

/// Remove a saved port-based database. This forgets only its connection
/// details; it never reaches the database server itself.
#[derive(Clone, Default, PartialEq, Debug, Action)]
#[action(namespace = ducktable, no_json)]
pub struct RemoveRemoteDatabase {
    pub name: String,
}

/// ⌥←/⌥→: the previous/next table in the sidebar (App::step_table).
/// App-level, like the view keys, so it works from any view; text
/// inputs stay safe — their own alt-arrow bindings (word jump) sit
/// deeper in the context stack and win while typing. The ⌥-arrow
/// grammar: ↑/↓ pages within a table, ←/→ circles the tables (with
/// rollover); views are ⌘1/⌘2/⌘3's job.
fn step_table(delta: i32, cx: &mut App) {
    on_view_in(cx, move |this, window, cx| this.step_table(delta, window, cx));
}

/// The window's view, for App-level handlers (menus fire at App level when
/// focus skips the view).
fn app_view(cx: &App) -> Option<Entity<DuckTable>> {
    cx.try_global::<AppView>().and_then(|v| v.0.upgrade())
}

/// Run `f` on the view on the next tick: a key or menu action arrives
/// inside the window's own update, so the view is touched after it, never
/// re-entrantly.
fn on_view(cx: &mut App, f: impl FnOnce(&mut DuckTable, &mut Context<DuckTable>) + 'static) {
    if let Some(view) = app_view(cx) {
        cx.defer(move |cx| view.update(cx, f));
    }
}

/// `on_view`, with the active window.
fn on_view_in(cx: &mut App, f: impl FnOnce(&mut DuckTable, &mut Window, &mut Context<DuckTable>) + 'static) {
    let Some(view) = app_view(cx) else { return };
    cx.defer(move |cx| {
        if let Some(w) = cx.active_window() {
            w.update(cx, |_, window, cx| view.update(cx, |this, cx| f(this, window, cx))).ok();
        }
    });
}

/// Run an Edit-menu row command only when the Data table itself owns
/// focus. App-level registration makes native menu items work; the grid
/// gate keeps the same shortcuts inert in Query, Structure, filters, and
/// cell editors.
fn run_row_action(action: fn(&mut grid::Grid, &mut Window, &mut Context<grid::Grid>), cx: &mut App) {
    on_view_in(cx, move |this, window, cx| {
        // Under the quit dialog no row is staged.
        if this.asking_to_quit {
            return;
        }
        let Some(grid) = this.grid.clone() else { return };
        if grid.read(cx).accepts_row_commands(window, cx) {
            grid.update(cx, |grid, cx| action(grid, window, cx));
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
    // Under the quit dialog the view stays: landing on one hands it the
    // keyboard, and Return would then type into the editor behind the
    // dialog, ⌘Enter run a statement there, and ⌘N stage a row.
    if asking_to_quit(cx) {
        return;
    }
    prefs::toggle(cx, |p| p.view = next);
    // Landing on Data hands focus back to the table; landing on Query
    // hands it to the editor — the same symmetry (docs/QUERY.md).
    on_view(cx, move |this, cx| match next {
        ViewMode::Data => {
            if let Some(grid) = &this.grid {
                grid.update(cx, |grid, cx| grid.request_focus(cx));
            }
        }
        ViewMode::Query => this.focus_query(cx),
        ViewMode::Structure => {}
    });
}

/// The one window's root view, for App-level action handlers that need
/// to reach into it (menus fire at App level when focus skips the view).
struct AppView(WeakEntity<DuckTable>);

impl Global for AppView {}

/// The macOS menu bar. The first menu becomes the application menu; the
/// About item opens the platform's standard dialog (window.prompt ->
/// NSAlert) with the version and a GitHub link. Check for Updates sits
/// under it in the conventional slot, and only when this build can update
/// itself (docs/UPDATES.md) — a dev bundle has no such item to disappoint.
fn app_menus(can_update: bool) -> Vec<Menu> {
    let mut app_menu = vec![MenuItem::action("About DuckTable", About)];
    if can_update {
        app_menu.push(MenuItem::separator());
        app_menu.push(MenuItem::action("Check for Updates…", CheckForUpdates));
    }
    app_menu.push(MenuItem::separator());
    app_menu.push(MenuItem::action("Quit DuckTable", Quit));
    vec![
        Menu::new("DuckTable").items(app_menu),
        Menu::new("File").items([
            // The platform picker, then the same door a drop uses.
            // ⌘O advertises itself from the keymap binding.
            MenuItem::action("Open Database File…", OpenDatabase),
            MenuItem::action("Open Database URL…", OpenDatabaseUrl),
        ]),
        Menu::new("Edit").items([
            MenuItem::action("New Row", AddRow),
            MenuItem::action("Duplicate Row", DuplicateRow),
            MenuItem::action("Delete Row", DeleteRow),
        ]),
        // macOS shows each item's key equivalent from the keymap, so this
        // menu is also where the zoom shortcuts advertise themselves.
        Menu::new("View").items([
            // macOS renders the ⌘1/⌘2/⌘3 and ⌥←/⌥→ key
            // equivalents from the keymap bindings.
            MenuItem::action("Structure", View1),
            MenuItem::action("Data", View2),
            MenuItem::action("Query", View3),
            MenuItem::separator(),
            MenuItem::action("Refresh Tables", RefreshTables),
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
            MenuItem::action("Column Tooltips", ToggleColumnCards),
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
        ]),
    ]
}

/// Quit, or leave the connected database, stop a server or remove a saved
/// remote, asking first when that would lose something (docs/EDITING.md,
/// "Dialogs"): staged changes, text in an open cell editor, a commit or a
/// Query statement still in flight, a transaction open in the Query view.
/// ⌘Q, the menu's Quit and the window's close button all come here, and so
/// does the updater's Install and Relaunch, which quits too; so do a row
/// click, Open Database File or URL and a dropped file while another
/// database is connected, and the sidebar's Stop and Remove Database.
///
/// The dialog is the app's own, not the platform's alert, because Cancel
/// has to be its default. Measured on the alert GPUI builds: a first button
/// titled Cancel takes Esc and gives up Return, so the alert has no default,
/// and GPUI seats the keyboard focus on the other button, where Space
/// presses it. Here Return and Esc both go back, and the button that goes
/// ahead is no tab stop, so the keyboard cannot reach it: it answers only
/// to a click.
fn request_leave(leaving: app::Leaving, window: &mut Window, cx: &mut App) {
    use gpui_kit::component::WindowExt as _;
    use gpui_kit::component::button::{Button, ButtonVariants as _};
    use gpui_kit::component::dialog::{DialogClose, DialogFooter};

    let Some(view) = app_view(cx) else {
        leave(leaving, cx);
        return;
    };
    let quitting = matches!(leaving, app::Leaving::Quit | app::Leaving::Relaunch);
    // Under the dialog no database is left: Cancel must find it as it was.
    if !quitting && view.read(cx).asking_to_quit {
        return;
    }
    if !view.update(cx, |this, cx| this.settle_before(&leaving, cx)) {
        return;
    }
    if view.read(cx).risks(&leaving, cx).question_for(&leaving).is_none() {
        go_ahead(&view, leaving, window, cx);
        return;
    }
    if view.read(cx).asking_to_quit {
        return;
    }
    view.update(cx, |this, cx| this.quit_dialog_opened(cx));
    window.open_dialog(cx, move |dialog, _, cx| {
        // What is at risk is read each time the dialog is drawn, not once
        // when it opened: a commit that settles under it has by then landed
        // or kept its edits, and the text follows.
        let question = view
            .read(cx)
            .risks(&leaving, cx)
            .question_for(&leaving)
            .unwrap_or_else(|| leaving.plain_question());
        // Going back, by Return, Esc or the Cancel button.
        let stay = {
            let view = view.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                view.update(cx, |this, cx| this.quit_dialog_cancelled(window, cx));
                true
            }
        };
        let quit = {
            let (view, leaving) = (view.clone(), leaving.clone());
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                // A quit takes the window with it; anything else goes on
                // without the dialog.
                if !quitting {
                    window.close_dialog(cx);
                }
                go_ahead(&view, leaving.clone(), window, cx);
            }
        };
        dialog
            .title(question.message.clone())
            .overlay_closable(false)
            .close_button(false)
            .child(div().text_sm().child(question.detail.clone()))
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("quit")
                            .label(question.confirm)
                            .danger()
                            .tab_stop(false)
                            .on_click(quit),
                    )
                    // DialogClose fills the width it is given, so it sits in a
                    // box of its own that is only as wide as the button.
                    .child(div().child(DialogClose::new().child(Button::new("cancel").label("Cancel").primary()))),
            )
            .on_ok(stay.clone())
            .on_cancel(stay)
    });
}

/// Do what was asked about, or needed no asking: end the app, or leave a
/// database through the view. Ending the app gives back the window's
/// sessions on the way out (`on_app_quit`), as every way out does; a
/// relaunch with no update waiting ends nothing, and keeps them.
fn go_ahead(view: &Entity<DuckTable>, leaving: app::Leaving, window: &mut Window, cx: &mut App) {
    match leaving {
        app::Leaving::Quit | app::Leaving::Relaunch => leave(leaving, cx),
        leaving => view.update(cx, |this, cx| this.go_ahead(leaving, window, cx)),
    }
}

/// End the app the way it was asked to: quit, or let the updater install
/// and relaunch. With no update waiting, a relaunch has nothing to do, and
/// leaving a database is the view's (`go_ahead`).
fn leave(leaving: app::Leaving, cx: &mut App) {
    match leaving {
        app::Leaving::Quit => cx.quit(),
        app::Leaving::Relaunch => {
            if let Some(updater) = &cx.global::<updater::UpdaterState>().0 {
                updater.install();
            }
        }
        _ => {}
    }
}

/// Ask through the window, from an App-level action, the updater, or a
/// view that cannot open the dialog from inside its own update. Deferred,
/// like every action that touches the window: a key or menu action arrives
/// inside the window's own update.
fn leave_asking(leaving: app::Leaving, cx: &mut App) {
    cx.defer(move |cx| {
        let window = cx.active_window().or_else(|| cx.windows().first().copied());
        match window {
            Some(w) => {
                w.update(cx, |_, window, cx| request_leave(leaving, window, cx)).ok();
            }
            None => leave(leaving, cx),
        }
    });
}

/// Whether the quit dialog is on screen. The menu bar and its keys still
/// work under it, and the actions that would replace the connected database
/// check this first.
fn asking_to_quit(cx: &App) -> bool {
    app_view(cx).is_some_and(|view| view.read(cx).asking_to_quit)
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
            cx.update(|cx| cx.open_url("https://github.com/shreeve/duckdb-harbor"));
        }
    })
    .detach();
}

/// `IconName` resolves to `icons/*.svg` asset paths, and the app serves
/// them. DuckTable's own icons are embedded here (Lucide, the set those
/// names come from) and answer first; any other path falls through to the
/// kit's default icon set, which holds the icons the components draw for
/// themselves (an input's clear button, a dialog's close). A path neither
/// has renders as an invisible-but-clickable control.
struct Assets;

macro_rules! icon {
    ($name:literal) => {
        (concat!("icons/", $name, ".svg"), include_bytes!(concat!("../../../assets/icons/", $name, ".svg")) as &[u8])
    };
}

const ICONS: [(&str, &[u8]); 13] = [
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
    icon!("plus"),
];

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        if let Some((_, bytes)) = ICONS.iter().find(|(p, _)| *p == path) {
            return Ok(Some((*bytes).into()));
        }
        Ok(gpui_kit::assets::Assets.load(path).ok().flatten())
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
            // Drop a database file anywhere on the window: the same door
            // as File→Open. First path wins — a multi-file drop opening N
            // databases would be N-1 surprises.
            .on_drop(cx.listener(|this, dropped: &ExternalPaths, _, cx| {
                if let Some(path) = dropped.paths().first().cloned() {
                    leave_asking(this.switch_to(app::Aim::File(path)), cx);
                }
            }))
            // The sidebar/content divider drags rightward from its
            // classic fixed width, the floor: the divider only grants more
            // room. It persists like the inspector's does.
            .child(
                gpui_kit::base::h_resizable("root-split")
                    .with_state(&self.sidebar_resize)
                    .child(
                        gpui_kit::component::resizable::resizable_panel()
                            .size(px(prefs::get(cx).sidebar_width))
                            .size_range(px(prefs::SIDEBAR_MIN)..px(prefs::SIDEBAR_MAX))
                            // Furniture: only the user's drag changes
                            // this width — a window resize gives all
                            // its delta to the content.
                            .fixed()
                            .child(self.sidebar(cx)),
                    )
                    .child(
                        gpui_kit::component::resizable::resizable_panel()
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
    let app = gpui_kit::application().with_assets(Assets);

    app.run(move |cx| {
        gpui_kit::init(cx);
        grid::init(cx);
        // The DuckDB grammar (crates/duckdb-lang), registered before any
        // editor renders: the Query view asks for language "duckdb".
        gpui_kit::component::highlighter::LanguageRegistry::singleton().register(
            "duckdb",
            &gpui_kit::component::highlighter::LanguageConfig::new(
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
        // Sparkle, from the framework the bundle embeds. None in debug
        // builds and bare `cargo run` binaries, and the menu item goes
        // with it.
        let updater = updater::Updater::init();
        let can_update = updater.is_some();
        cx.set_global(updater::UpdaterState(updater));
        cx.on_action(|_: &CheckForUpdates, cx| {
            if let Some(updater) = &cx.global::<updater::UpdaterState>().0 {
                // An update held from an earlier Install and Relaunch is
                // offered again here: Sparkle would not hold it twice.
                if updater.install_waiting() {
                    leave_asking(app::Leaving::Relaunch, cx);
                } else {
                    updater.check_for_updates();
                }
            }
        });
        // Install and Relaunch quits the app, so it asks what ⌘Q asks
        // before Sparkle goes on (`updater.rs`).
        if let Some(updater) = &cx.global::<updater::UpdaterState>().0 {
            let requests = updater.relaunch_requests();
            cx.spawn(async move |cx| {
                while requests.recv().await.is_ok() {
                    cx.update(|cx| leave_asking(app::Leaving::Relaunch, cx));
                }
            })
            .detach();
        }
        cx.bind_keys([
            KeyBinding::new("cmd-i", ToggleInspector, None),
            KeyBinding::new("cmd-o", OpenDatabase, None),
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-r", RefreshTables, None),
            KeyBinding::new("cmd-n", AddRow, None),
            KeyBinding::new("cmd-d", DuplicateRow, None),
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
            KeyBinding::new("cmd-t", ToggleColumnCards, None),
        ]);
        // File→Open: the platform picker, then app.open_path — the same
        // door a drag-drop uses. .duckdb is what it speaks today; the
        // open-anything dispatcher (CSV, Parquet, Sheets URLs…) grows on
        // this trunk. The picker offers no extension filter (gpui's
        // PathPromptOptions has none), and none is enforced here: a wrong
        // file fails honestly in the connect card with harbor's own error.
        cx.on_action(|_: &OpenDatabase, cx| {
            // Under the quit dialog a database is not opened: Cancel must
            // find everything as it was.
            if asking_to_quit(cx) {
                return;
            }
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
                        if let Some(view) = app_view(cx) {
                            let leaving = view.read(cx).switch_to(app::Aim::File(path));
                            leave_asking(leaving, cx);
                        }
                    });
                }
            })
            .detach();
        });
        cx.on_action(|_: &OpenDatabaseUrl, cx| {
            if asking_to_quit(cx) {
                return;
            }
            let view = cx.try_global::<AppView>().map(|v| v.0.clone());
            cx.defer(move |cx| {
                let Some(view) = view else { return };
                if let Some(w) = cx.active_window() {
                    w.update(cx, |_, window, cx| add_database::open(view, window, cx))
                        .ok();
                }
            });
        });
        // Right-click → Stop and Remove Database ask first when the
        // database holds something to lose (`request_leave`).
        cx.on_action(|a: &StopBerth, cx| {
            leave_asking(app::Leaving::Stop { name: a.name.clone(), path: a.path.clone() }, cx);
        });
        cx.on_action(|a: &StartBerth, cx| {
            let path = a.path.clone();
            on_view(cx, move |this, cx| this.start_berth(path, cx));
        });
        cx.on_action(|a: &AttachBerth, cx| {
            let path = a.path.clone();
            on_view(cx, move |this, cx| this.attach_berth(path, cx));
        });
        cx.on_action(|a: &DetachBerth, cx| {
            let path = a.path.clone();
            on_view(cx, move |this, cx| this.detach_berth(path, cx));
        });
        cx.on_action(|a: &ToggleAutostart, cx| {
            let (path, on) = (a.path.clone(), a.on);
            on_view(cx, move |this, cx| this.toggle_autostart(path, on, cx));
        });
        cx.on_action(|a: &RemoveRemoteDatabase, cx| {
            leave_asking(app::Leaving::Remove { name: a.name.clone() }, cx);
        });
        cx.on_action(|_: &TablePrev, cx| step_table(-1, cx));
        cx.on_action(|_: &TableNext, cx| step_table(1, cx));
        cx.on_action(|_: &AddRow, cx| run_row_action(grid::Grid::add_row, cx));
        cx.on_action(|_: &DuplicateRow, cx| {
            run_row_action(grid::Grid::duplicate_row, cx)
        });
        cx.on_action(|_: &DeleteRow, cx| {
            run_row_action(grid::Grid::delete_row, cx)
        });
        // One command behind both the View menu and Cmd+R. The sidebar
        // glyph calls the same refresh_tables method directly from its
        // DuckTable context, so every entrance has identical semantics.
        cx.on_action(|_: &RefreshTables, cx| on_view(cx, |this, cx| this.refresh_tables(cx)));
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
        cx.on_action(|_: &Quit, cx| leave_asking(app::Leaving::Quit, cx));
        // One window, so closing it is quitting. macOS lets an app outlive
        // its windows, which suits a document app whose File menu can open
        // another; DuckTable's menus act on the window that is gone, so a
        // bare menu bar would be a dead app still holding the Dock. The
        // close button asks what Quit asks (`on_window_should_close` below)
        // before the window goes.
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
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
        // Global, like every app-wide action: the menu item validates by
        // it, and the key reaches it from wherever focus is.
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
        // The column card under an edited cell and on a draft
        // placeholder (grid.rs column_card).
        cx.on_action(|_: &ToggleColumnCards, cx| {
            prefs::toggle(cx, |p| p.column_cards = !p.column_cards);
        });
        // FitColumns reaches the window's grid through the view, never by
        // dispatching into the window again: with focus in a text input
        // the window's listeners are off the dispatch path, so a dispatch
        // would come straight back here, deferred again forever.
        cx.on_action(|_: &FitColumns, cx| {
            on_view(cx, |this, cx| {
                if let Some(grid) = &this.grid {
                    grid.update(cx, |grid, cx| grid.fit_columns(cx));
                }
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
        cx.set_menus(app_menus(can_update));

        // The window opens where it last stood — the frame saved on
        // every move and resize below. A fresh install has no frame
        // and takes the platform default.
        let remembered = prefs::get(cx).win.map(|(x, y, w, h)| {
            WindowBounds::Windowed(Bounds::new(
                point(px(x), px(y)),
                size(px(w), px(h)),
            ))
        });
        gpui_kit::open_window(
            WindowOptions {
                window_min_size: Some(size(px(720.), px(420.))),
                window_bounds: remembered,
                ..Default::default()
            },
            cx,
            |window, cx| {
                let view = cx.new(DuckTable::new);
                // The one window's view, reachable from App-level
                // action handlers (FitColumns above).
                cx.set_global(AppView(view.downgrade()));
                // Closing the window is quitting, so it asks the same
                // question: with something to lose the window stays, and
                // the dialog's answer decides.
                window.on_window_should_close(cx, |window, cx| {
                    let at_risk =
                        app_view(cx).is_some_and(|view| view.read(cx).quit_risks(cx).question().is_some());
                    if at_risk {
                        request_leave(app::Leaving::Quit, window, cx);
                    }
                    !at_risk
                });
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
                view
            },
        )
        .expect("failed to open the DuckTable window");
    });
}
