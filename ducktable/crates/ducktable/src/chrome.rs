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
    let active_weight = FontWeight(560.);
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
                d.bg(t.accent)
            } else {
                d.hover(|d| d.bg(t.row_hover))
            }
        })
        .on_click(move |e, window, cx| handler(e, window, cx))
        // Constant width in every state: an invisible ghost of the label
        // at the ACTIVE weight owns the layout, and the visible label
        // sits on top of it. Bold-on-select can then never widen the
        // segment, so the track never shifts (the footer's no-jitter
        // rule, applied to the switcher itself).
        .child(
            div()
                .relative()
                .child(
                    div()
                        .font_weight(active_weight)
                        .text_color(gpui::transparent_black())
                        .child(label),
                )
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .map(|d| {
                            if on {
                                d.text_color(t.on_accent).font_weight(active_weight)
                            } else {
                                d.text_color(t.muted)
                            }
                        })
                        .child(label),
                ),
        )
}

/// The hairline between segments: always present, full height, the same
/// 1px and color as the track's outer border — the interior lines speak
/// the exact grammar the outer frame already does where it meets the
/// fill. Nothing about it ever changes state, so nothing can flicker;
/// only the accent fill moves when the selection does.
pub(crate) fn seg_sep(t: Pal) -> Div {
    div().w(px(1.)).h_full().flex_none().bg(t.border)
}

/// The one duration for an in-place micro-fade — an icon swap, a small
/// element breathing in or out. ~150ms: above the ~100ms motion floor,
/// below the ~250ms where a tiny element feels slow (see docs/UI.md,
/// Motion). Shared so the copy tile's crossfade and the stop spinner's
/// fade-in can't drift apart. NOT for structural enter/leave (a whole row
/// departing runs a hair longer), loops (a spinner's turn), or dwell
/// timers (how long a confirmation holds to be read) — those measure
/// different things and keep their own numbers.
pub(crate) const QUICK_FADE_MS: u64 = 150;

/// Crossfade: two elements stacked on ONE clock, opposite directions —
/// the outgoing breathes out exactly as the incoming breathes in. An
/// instant vanish beside a fade-in reads as a glitch; this is the
/// reusable cure (first used by the copy tile's check -> copy revert).
///
/// The caller renders this during a crossfade WINDOW (a state its own
/// timer closes a hair after `ms`), keys it with a sequence number so
/// each playback runs once, and must SIZE the returned container — the
/// children are absolute, so it has no intrinsic size of its own.
pub(crate) fn crossfade<A, B>(name: &'static str, seq: u64, ms: u64, out: A, in_: B) -> Div
where
    A: Element + Styled,
    B: Element + Styled,
{
    let dur = std::time::Duration::from_millis(ms);
    div()
        .relative()
        .child(div().absolute().inset_0().child(out.with_animation(
            ElementId::from((name, seq << 1)),
            Animation::new(dur),
            |el, delta| el.opacity(1.0 - delta),
        )))
        .child(div().absolute().inset_0().child(in_.with_animation(
            ElementId::from((name, (seq << 1) | 1)),
            Animation::new(dur),
            |el, delta| el.opacity(delta),
        )))
}
