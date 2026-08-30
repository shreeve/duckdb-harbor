//! DuckTable: a fast, minimal desktop client for DuckDB, speaking to
//! DuckDB Harbor. This file is only the entry point; each surface owns a
//! file (`app.rs` state, `sidebar.rs`, `content.rs`, `theme.rs`).

mod app;
mod content;
mod grid;
mod inspector;
mod prefs;
mod sidebar;
mod theme;
mod util;

use app::DuckTable;
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::{Root, StyledExt as _};

actions!(ducktable, [ToggleInspector]);

impl Render for DuckTable {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let show_inspector =
            prefs::get(cx).inspector && matches!(self.phase, app::Phase::Connected { .. });
        div()
            .size_full()
            .h_flex()
            .font_family(theme::ui_font())
            .on_action(cx.listener(|_, _: &ToggleInspector, _, cx| {
                prefs::toggle(cx, |p| p.inspector = !p.inspector);
            }))
            .child(self.sidebar(cx))
            .child(self.content(cx))
            .when(show_inspector, |d| d.child(self.inspector(cx)))
    }
}

fn main() {
    let app = Application::new();

    app.run(move |cx| {
        gpui_component::init(cx);
        theme::init(cx);
        prefs::init(cx);
        cx.bind_keys([KeyBinding::new("cmd-alt-0", ToggleInspector, None)]);

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
