//! File → Open Database URL: save a Harbor host and port under a sidebar name.
//! Localhost connects directly; any other host is reached through SSH.

use crate::app::DuckTable;
use gpui::{
    App, AppContext as _, ParentElement as _, Styled as _, WeakEntity, Window, div,
    prelude::FluentBuilder as _,
};
use gpui_component::dialog::DialogButtonProps;
use gpui_component::form::{field, v_form};
use gpui_component::input::{Input, InputState};
use gpui_component::{ActiveTheme as _, WindowExt as _};
use std::cell::RefCell;
use std::rc::Rc;

pub(crate) fn open(view: WeakEntity<DuckTable>, window: &mut Window, cx: &mut App) {
    let name = cx.new(|cx| InputState::new(window, cx).placeholder("Production"));
    let host = cx.new(|cx| InputState::new(window, cx).default_value("localhost"));
    let port = cx.new(|cx| InputState::new(window, cx).default_value("9495"));
    let error = Rc::new(RefCell::new(None::<String>));

    window.open_dialog(cx, {
        let name = name.clone();
        let host = host.clone();
        let port = port.clone();
        let error = Rc::clone(&error);
        move |dialog, _window, cx| {
            let form = v_form()
                .child(
                    field()
                        .label("Name")
                        .description("How this database appears in the sidebar")
                        .required(true)
                        .child(Input::new(&name)),
                )
                .child(
                    field()
                        .label("Host")
                        .description("localhost connects directly; another host connects over SSH")
                        .required(true)
                        .child(Input::new(&host)),
                )
                .child(
                    field()
                        .label("Harbor port")
                        .description("The Harbor listener on that host")
                        .required(true)
                        .child(Input::new(&port)),
                );

            dialog
                .title("Open Database URL")
                .confirm()
                .button_props(DialogButtonProps::default().ok_text("Open Database"))
                .child(form)
                .when_some(error.borrow().clone(), |dialog, message| {
                    dialog.child(div().text_sm().text_color(cx.theme().danger).child(message))
                })
                .on_ok({
                    let name = name.clone();
                    let host = host.clone();
                    let port = port.clone();
                    let error = Rc::clone(&error);
                    let view = view.clone();
                    move |_, window, cx| {
                        let name = name.read(cx).value().to_string();
                        let host = host.read(cx).value().to_string();
                        let port = port.read(cx).value().to_string();
                        if let Err(message) = harbor_client::fleet::validate_database(
                            &name,
                            &host,
                            &port,
                        ) {
                            *error.borrow_mut() = Some(message);
                            window.refresh();
                            return false;
                        }
                        if let Some(view) = view.upgrade() {
                            view.update(cx, |app, cx| {
                                app.add_database(name, host, port, cx);
                            });
                        }
                        true
                    }
                })
        }
    });
}
