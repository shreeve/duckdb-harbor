//! A copy-to-clipboard tile that confirms itself: click it and the glyph
//! flips to a green check (tooltip "Copied") until its own timer brings
//! the copy glyph back. Self-contained on purpose — the host hands it the
//! text and drops the entity into a layout; state, timers, and clipboard
//! stay inside.
//!
//! Liftable to another app with `theme::pal` and the `icon_tile` chassis
//! (grid.rs) it renders with, plus the copy/check icon assets.

use crate::theme::pal;
use gpui::*;

/// The confirmation's life: the check lands same-frame (feedback is
/// content, it snaps), holds, then CROSSFADES home — check breathing out
/// while the copy glyph breathes in, together, because an instant
/// vanish beside a fade-in reads as a glitch.
#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Rest,
    Copied,
    Crossfade,
}

// How long the green check DWELLS before reverting — a readability budget,
// not a motion one (see docs/UI.md, Motion). You initiated the copy and are
// looking right at it, so you register the check in <100ms; ~1.2s is
// un-missable without overstaying (1.6s + the fade read as "stuck").
const HOLD_MS: u64 = 1200;
// The revert crossfade is an in-place micro-fade: one shared duration with
// the rest of the app's small fades, so they can't drift.
const FADE_MS: u64 = crate::chrome::QUICK_FADE_MS;

pub(crate) struct CopyButton {
    /// Tooltip label in the resting state ("Copy DDL").
    label: &'static str,
    text: String,
    phase: Phase,
    /// A rapid re-copy restarts the flash; only its own timers advance it.
    seq: u64,
}

impl CopyButton {
    pub(crate) fn new(label: &'static str, text: String) -> Self {
        Self { label, text, phase: Phase::Rest, seq: 0 }
    }
}

impl Render for CopyButton {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = pal(cx);
        let phase = self.phase;
        let label = self.label;
        let text = self.text.clone();
        let check = || svg().path("icons/check.svg").size_3().text_color(t.good);
        let copy = || svg().path("icons/copy.svg").size_3().text_color(t.muted);
        crate::chrome::icon_tile("copy", 20., true, t)
            .tooltip(move |window, cx| {
                gpui_component::tooltip::Tooltip::new(if phase == Phase::Rest {
                    label
                } else {
                    "Copied"
                })
                .build(window, cx)
            })
            .child(match phase {
                Phase::Rest => copy().into_any_element(),
                Phase::Copied => check().into_any_element(),
                Phase::Crossfade => crate::chrome::crossfade(
                    "copy-revert", self.seq, FADE_MS, check(), copy(),
                )
                .size_3()
                .into_any_element(),
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                this.phase = Phase::Copied;
                this.seq += 1;
                let seq = this.seq;
                cx.notify();
                cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(HOLD_MS))
                        .await;
                    let live = this
                        .update(cx, |b, cx| {
                            if b.seq == seq {
                                b.phase = Phase::Crossfade;
                                cx.notify();
                                true
                            } else {
                                false
                            }
                        })
                        .unwrap_or(false);
                    if !live {
                        return;
                    }
                    // A hair past the fade, so the animation finishes
                    // before the solid resting glyph takes over.
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(FADE_MS + 50))
                        .await;
                    this.update(cx, |b, cx| {
                        if b.seq == seq {
                            b.phase = Phase::Rest;
                            cx.notify();
                        }
                    })
                    .ok();
                })
                .detach();
            }))
    }
}
