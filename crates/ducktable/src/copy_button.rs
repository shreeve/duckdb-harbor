//! A copy-to-clipboard tile that confirms itself: click it and the glyph
//! flips to a green check (tooltip "Copied") until its own timer flips it
//! back. Self-contained on purpose — the host hands it the text and drops
//! the entity into a layout; state, timer, and clipboard stay inside.
//!
//! Liftable to another app with `theme::pal` and the `icon_tile` chassis
//! (grid.rs) it renders with, plus the copy/check icon assets.

use crate::theme::pal;
use gpui::*;

pub(crate) struct CopyButton {
    /// Tooltip label in the resting state ("Copy DDL").
    label: &'static str,
    text: String,
    copied: bool,
    /// A rapid re-copy restarts the flash; only its own timer ends it.
    seq: u64,
}

impl CopyButton {
    pub(crate) fn new(label: &'static str, text: String) -> Self {
        Self { label, text, copied: false, seq: 0 }
    }
}

impl Render for CopyButton {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = pal(cx);
        let copied = self.copied;
        let label = self.label;
        let text = self.text.clone();
        crate::chrome::icon_tile("copy", 20., true, t)
            .tooltip(move |window, cx| {
                gpui_component::tooltip::Tooltip::new(if copied { "Copied" } else { label })
                    .build(window, cx)
            })
            .child(
                // Raw svg() does NOT inherit text color.
                svg()
                    .path(if copied { "icons/check.svg" } else { "icons/copy.svg" })
                    .size_3()
                    .text_color(if copied { t.good } else { t.muted }),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                this.copied = true;
                this.seq += 1;
                let seq = this.seq;
                cx.notify();
                cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(1600))
                        .await;
                    this.update(cx, |b, cx| {
                        if b.seq == seq {
                            b.copied = false;
                            cx.notify();
                        }
                    })
                    .ok();
                })
                .detach();
            }))
    }
}
