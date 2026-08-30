//! The grid's bottom bar (UI.md "Bottom bar", design.css `.bbar`):
//! view switcher, filter toggle, Columns popover, pager, and the
//! right-anchored status line. An `impl Grid` satellite, the same shape
//! as `inspector.rs` and `structure.rs` — per-table controls, a
//! different scope from the header strip's global display prefs.

use crate::grid::{icon_tile, Grid, ViewMode};
use crate::theme::pal;
use crate::util::commas;
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::button::ButtonVariants as _;
use gpui_component::tooltip::Tooltip;
use gpui_component::{Sizable as _, StyledExt as _};

impl Grid {
    pub(crate) fn footer(&self, cx: &mut Context<Self>) -> Div {
        let t = pal(cx);
        let view = self.view();
        let (count, cols, loading) = self.table_facts(cx);
        let can_prev = self.page > 0;
        let can_next = self.has_next(cx);
        let can_last = matches!(self.last_page(), Some(lp) if self.page < lp);
        let filter_open = self.filter_input.is_some();
        let base = self.page * self.page_size;
        let (first, last) = (base + 1, base + count);
        let rows_part = if count == 0 {
            "0 rows".to_string()
        } else {
            match self.total_rows {
                Some(t) => format!(
                    "{}\u{2013}{} of {} rows",
                    commas(first as u64),
                    commas(last as u64),
                    commas(t)
                ),
                None => {
                    format!("{}\u{2013}{} rows", commas(first as u64), commas(last as u64))
                }
            }
        };
        // Ordering rule for a jitter-free footer: in a right-justified
        // cluster an element only moves when something to its RIGHT
        // changes width. The pager — the only interactive element — is
        // therefore RIGHTMOST: constant-width glyphs pinned to the
        // corner, so neither page flips nor table switches ever move the
        // click targets ("N per" grows only when the user cycles it).
        // "N columns" sits just left of it, beside the row range it
        // describes; the per-page variables (ms, range) stay leftmost.
        // Table switches shift only text whose content changed anyway.
        // The columns text is its OWN node — as a suffix of one longer
        // string its glyphs land a subpixel differently and the view
        // switch shows a 1px shift.
        let columns_part =
            format!("{} {}", cols, if cols == 1 { "column" } else { "columns" });
        let loading_empty = loading && count == 0;
        let pager_visible = view == ViewMode::Data && !loading_empty;
        let status_prefix = match view {
            ViewMode::Data if loading_empty => Some("loading...".to_string()),
            ViewMode::Data => {
                Some(format!("{} ms \u{00b7} {rows_part}", self.last_time_ms))
            }
            ViewMode::Structure => None,
        };
        let status_columns =
            (view == ViewMode::Structure || pager_visible).then_some(columns_part);
        div()
            .h_flex()
            .h(px(38.))
            .flex_none()
            .items_center()
            .px(px(10.))
            .bg(t.raised)
            .border_t_1()
            .border_color(t.border)
            .child(
                // design.css `.seg`: the active fill runs flush to the
                // track's edges. gpui does not clip child backgrounds to
                // the track's radius, so each end segment carries its own
                // matching outer corners (nested radius = track radius -
                // border).
                div()
                    .h_flex()
                    .flex_none()
                    .rounded(px(8.))
                    .bg(t.surface)
                    .border_1()
                    .border_color(t.border)
                    .child(seg_tile(
                        "view-data",
                        "Data",
                        view == ViewMode::Data,
                        (true, false),
                        t,
                        cx.listener(|this, _, _, cx| {
                            this.set_view(ViewMode::Data);
                            cx.notify();
                        }),
                    ))
                    .child(seg_tile(
                        "view-structure",
                        "Structure",
                        view == ViewMode::Structure,
                        (false, true),
                        t,
                        cx.listener(|this, _, _, cx| {
                            this.set_view(ViewMode::Structure);
                            cx.notify();
                        }),
                    )),
            )
            .when(view == ViewMode::Data, |d| {
                // The filter toggle sits by the view switcher; accent
                // when a filter is ACTIVE, not just open.
                d.child(
                    icon_tile("toggle-filter", 22., true, t)
                        .ml_2()
                        .tooltip(|window, cx| {
                            Tooltip::new("Filter (raw SQL WHERE)").build(window, cx)
                        })
                        .child(
                            svg()
                                .path("icons/funnel.svg")
                                .size_3p5()
                                .text_color(if self.filter.is_some() || filter_open {
                                    t.accent
                                } else {
                                    t.muted
                                }),
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.toggle_filter_strip(window, cx);
                        })),
                )
            })
            .when(view == ViewMode::Data, |d| {
                d.child(self.columns_popover(cx))
            })
            .child(div().flex_1())
            .child(
                // One right-anchored line: ms · range · columns · pager
                // (see the ordering rule above).
                div()
                    .ml_2()
                    .h_flex()
                    .flex_none()
                    .items_center()
                    .gap_2()
                    .text_xs()
                    .text_color(t.muted)
                    .when_some(status_prefix, |d, text| d.child(div().child(text)))
                    .when_some(status_columns, |d, text| {
                        d.when(view == ViewMode::Data, |d| d.child(div().child("\u{00b7}")))
                            .child(div().child(text))
                    })
                    .when(pager_visible, |d| {
                        let arrow = |id: &'static str,
                                     path: &'static str,
                                     enabled: bool| {
                            icon_tile(id, 20., enabled, t)
                                .text_color(if enabled {
                                    t.text
                                } else {
                                    t.muted.opacity(0.4)
                                })
                                .child(gpui_component::Icon::empty().path(path).size_4())
                        };
                        d.child(div().child("\u{00b7}"))
                            .child(
                                div()
                                    .h_flex()
                                    .items_center()
                                    .gap_0p5()
                                    .child(
                                        arrow(
                                            "page-first",
                                            "icons/chevron-first.svg",
                                            can_prev,
                                        )
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.jump_first(cx)
                                        })),
                                    )
                                    .child(
                                        arrow(
                                            "page-prev",
                                            "icons/chevron-left.svg",
                                            can_prev,
                                        )
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.prev_page(cx)
                                        })),
                                    )
                                    .child(
                                        div()
                                            .id("page-size")
                                            .px_1()
                                            .h(px(20.))
                                            .h_flex()
                                            .items_center()
                                            .rounded(px(4.))
                                            .cursor_pointer()
                                            .hover(|d| d.bg(t.row_hover))
                                            .tooltip(|window, cx| {
                                                Tooltip::new(
                                                    "Rows per page \u{2014} \
                                                     click to change",
                                                )
                                                .build(window, cx)
                                            })
                                            .child(format!("{} per", commas(self.page_size as u64)))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.cycle_page_size(cx);
                                            })),
                                    )
                                    .child(
                                        arrow(
                                            "page-next",
                                            "icons/chevron-right.svg",
                                            can_next,
                                        )
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.next_page(cx)
                                        })),
                                    )
                                    .child(
                                        arrow(
                                            "page-last",
                                            "icons/chevron-last.svg",
                                            can_last,
                                        )
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.jump_last(cx)
                                        })),
                                    ),
                            )
                    }),
            )
    }

    /// Column show/hide, in a popover that stays open across toggles.
    fn columns_popover(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let grid = cx.entity();
        gpui_component::popover::Popover::new("columns-popover")
            .anchor(Corner::BottomLeft)
            .trigger(
                gpui_component::button::Button::new("columns-btn")
                    .icon(gpui_component::IconName::Eye)
                    .ghost()
                    .xsmall()
                    .tooltip("Show or hide columns"),
            )
            .content(move |_, _, cx| {
                let t = pal(cx);
                let list = grid.read(cx).column_list(cx);
                let total = list.len();
                let shown = list.iter().filter(|&&(_, _, h)| !h).count();
                let hidden_any = shown < total;
                // Same rule as the sidebar filters: a search box only
                // earns its row past 10 items.
                let searchable = total > 10;
                let search = grid.read(cx).col_search.clone();
                let query = search.read(cx).value().trim().to_lowercase();
                let matches: Vec<_> = list
                    .into_iter()
                    .filter(|(_, name, _)| {
                        query.is_empty() || name.to_lowercase().contains(&query)
                    })
                    .collect();
                let none = matches.is_empty();
                // The whole row is the click target; the Checkbox is
                // visual only (its handlerless listener no-ops and the
                // click bubbles to the row).
                let mut rows = div()
                    .id("columns-list")
                    .v_flex()
                    .p(px(4.))
                    .gap_px()
                    .max_h(px(340.))
                    .overflow_y_scroll();
                for (ix, name, hidden) in matches {
                    let grid = grid.clone();
                    rows = rows.child(
                        div()
                            .id(("colrow", ix))
                            .h_flex()
                            .items_center()
                            .gap_2()
                            .px(px(6.))
                            .py(px(3.))
                            .rounded(px(5.))
                            .cursor_pointer()
                            .hover(|d| d.bg(t.row_hover))
                            .child(
                                gpui_component::checkbox::Checkbox::new(("col", ix))
                                    .checked(!hidden)
                                    .small(),
                            )
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .map(|d| {
                                        if hidden {
                                            d.text_color(t.muted)
                                        } else {
                                            d.text_color(t.text)
                                        }
                                    })
                                    .child(name),
                            )
                            .on_click(move |_, _, cx| {
                                grid.update(cx, |g, cx| {
                                    g.toggle_column(ix, cx);
                                });
                            }),
                    );
                }
                // Header links stay put (dimmed when inapplicable) so
                // the row never reflows as columns toggle.
                let link = |id: &'static str, label: &'static str, enabled: bool| {
                    div()
                        .id(id)
                        .text_xs()
                        .map(|d| {
                            if enabled {
                                d.text_color(t.accent).cursor_pointer()
                            } else {
                                d.text_color(t.muted.opacity(0.5))
                            }
                        })
                        .child(label)
                };
                div()
                    .v_flex()
                    .w(px(250.))
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_2()
                            .px(px(10.))
                            .pt(px(8.))
                            .pb(px(6.))
                            .border_b_1()
                            .border_color(t.border)
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight(560.))
                                    .text_color(t.muted)
                                    .child("COLUMNS"),
                            )
                            .when(hidden_any, |d| {
                                d.child(
                                    div()
                                        .text_xs()
                                        .text_color(t.muted)
                                        .child(format!("{shown} of {total}")),
                                )
                            })
                            .child(div().flex_1())
                            .child(
                                link("cols-show-all", "Show all", hidden_any).when(
                                    hidden_any,
                                    |d| {
                                        let grid = grid.clone();
                                        d.on_click(move |_, _, cx| {
                                            grid.update(cx, |g, cx| {
                                                g.show_all_columns(cx);
                                            });
                                        })
                                    },
                                ),
                            )
                            .child(
                                link("cols-hide-all", "Hide all", shown > 1).when(
                                    shown > 1,
                                    |d| {
                                        let grid = grid.clone();
                                        d.on_click(move |_, _, cx| {
                                            grid.update(cx, |g, cx| {
                                                g.hide_all_columns(cx);
                                            });
                                        })
                                    },
                                ),
                            ),
                    )
                    .when(searchable, |d| {
                        d.child(
                            div().px(px(8.)).pt(px(8.)).child(
                                gpui_component::input::Input::new(&search)
                                    .xsmall()
                                    .cleanable(true),
                            ),
                        )
                    })
                    .when(none, |d| {
                        d.child(
                            div()
                                .px(px(10.))
                                .py(px(10.))
                                .text_xs()
                                .text_color(t.muted)
                                .child("No matching columns"),
                        )
                    })
                    .child(rows)
                    .into_any_element()
            })
    }
}

/// One segment of the footer's view switcher (design.css `.seg span`):
/// contiguous segments on a surface track, and the active one is a SOLID
/// accent fill with on-accent text — a true segmented control, bolder
/// than the header's independent display toggles.
fn seg_tile(
    id: &'static str,
    label: &'static str,
    on: bool,
    (first, last): (bool, bool),
    t: crate::theme::Pal,
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
