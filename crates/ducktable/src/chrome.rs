//! The tile family — the small interactive chrome every strip is built
//! from. One home so the next surface reuses a chassis instead of
//! growing a fifth variant; callers own their colors and handlers, these
//! own the geometry and hover grammar.

use crate::theme::Pal;
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::tooltip::Tooltip;
use gpui_component::StyledExt as _;

/// A square hover-highlight icon tile — the chassis every header/footer
/// glyph shares. Callers layer their own colors (state tints, disabled
/// dimming) on top; a disabled tile skips the pointer and hover.
pub(crate) fn icon_tile(id: &'static str, size: f32, enabled: bool, t: Pal) -> Stateful<Div> {
    div()
        .id(id)
        .h_flex()
        .items_center()
        .justify_center()
        .size(px(size))
        .rounded(px(4.))
        .when(enabled, move |d| d.cursor_pointer().hover(move |d| d.bg(t.row_hover)))
}

/// A small icon button for a sidebar section header (filter, refresh).
pub(crate) fn head_glyph(id: &'static str, on: bool, t: Pal) -> Stateful<Div> {
    div()
        .id(id)
        .h_flex()
        .items_center()
        .justify_center()
        .size(px(18.))
        .rounded(px(4.))
        .cursor_pointer()
        .text_color(if on { t.accent } else { t.muted })
        .hover(|d| d.bg(t.row_hover))
}

/// One tile in the display-toggle track: flat glyph when off, accent-tinted
/// fill when on, faint hover, tooltip on hover.
pub(crate) fn toggle_tile(
    id: &'static str,
    glyph: &'static str,
    tip: &'static str,
    on: bool,
    t: Pal,
    handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    div()
        .id(id)
        .h_flex()
        .items_center()
        .justify_center()
        .w(px(24.))
        .h(px(18.))
        .rounded(px(4.))
        .cursor_pointer()
        .text_size(px(11.))
        .map(|d| {
            if on {
                d.bg(t.accent.opacity(0.15)).text_color(t.accent)
            } else {
                d.text_color(t.muted)
            }
        })
        .hover(move |d| if on { d } else { d.bg(t.row_hover) })
        .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
        .on_click(move |e, window, cx| handler(e, window, cx))
        .child(glyph)
}

/// One segment of the footer's view switcher (design.css `.seg span`):
/// contiguous segments on a surface track, and the active one is a SOLID
/// accent fill with on-accent text — a true segmented control, bolder
/// than the header's independent display toggles.
pub(crate) fn seg_tile(
    id: &'static str,
    label: &'static str,
    on: bool,
    (first, last): (bool, bool),
    t: Pal,
    handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let r = px(7.);
    div()
        .id(id)
        .px(px(11.))
        .py(px(3.))
        .when(first, |d| d.rounded_tl(r).rounded_bl(r))
        .when(last, |d| d.rounded_tr(r).rounded_br(r))
        .cursor_pointer()
        .text_size(px(12.))
        .map(|d| {
            if on {
                d.bg(t.accent).text_color(t.on_accent).font_weight(FontWeight(560.))
            } else {
                d.text_color(t.muted).hover(|d| d.bg(t.row_hover))
            }
        })
        .on_click(move |e, window, cx| handler(e, window, cx))
        .child(label)
}
