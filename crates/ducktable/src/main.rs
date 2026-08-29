use gpui::*;
use gpui_component::{button::*, *};

pub struct DuckTable;

impl Render for DuckTable {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .v_flex()
            .gap_2()
            .size_full()
            .items_center()
            .justify_center()
            .child("DuckTable")
            .child(
                Button::new("connect")
                    .primary()
                    .label("Connect to a berth")
                    .on_click(|_, _, _| println!("connect")),
            )
    }
}

fn main() {
    let app = Application::new();

    app.run(move |cx| {
        gpui_component::init(cx);

        cx.spawn(async move |cx| {
            let options = WindowOptions {
                window_min_size: Some(size(px(640.), px(400.))),
                ..Default::default()
            };
            cx.open_window(options, |window, cx| {
                let view = cx.new(|_| DuckTable);
                cx.new(|cx| Root::new(view, window, cx))
            })?;

            Ok::<_, anyhow::Error>(())
        })
        .detach();
    });
}
