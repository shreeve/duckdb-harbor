//! The grid's bottom bar (UI.md "Bottom bar", design.css `.bbar`):
//! view switcher, filter toggle, Columns popover, pager, and the
//! right-anchored status line. An `impl Grid` satellite, the same shape
//! as `inspector.rs` and `structure.rs` — per-table controls, a
//! different scope from the header strip's global display prefs.

use crate::chrome::{icon_tile, seg_sep, seg_tile};
use crate::grid::Grid;
use crate::prefs::ViewMode;
use crate::theme::{pal, PANE_INSET};
use crate::util::commas;
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::button::ButtonVariants as _;
use gpui_component::tooltip::Tooltip;
use gpui_component::{Sizable as _, StyledExt as _};

/// A key value as the review popover shows it: strings bare, everything
/// else in its JSON spelling.
fn vtext(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// What the footer's right-anchored cluster renders from — computed off
/// whichever grid the current view shows: the Data view's own, or the
/// Query view's embedded results grid. Both are Grids; the footer does
/// not care which.
pub(crate) struct FooterFacts {
    pub(crate) ms: u64,
    pub(crate) count: usize,
    pub(crate) cols: usize,
    pub(crate) loading: bool,
    pub(crate) page: usize,
    pub(crate) page_size: usize,
    pub(crate) total: Option<u64>,
    pub(crate) can_prev: bool,
    pub(crate) can_next: bool,
    pub(crate) can_last: bool,
    pub(crate) pageable: bool,
}

impl Grid {
    pub(crate) fn footer_facts(&self, cx: &App) -> FooterFacts {
        let (count, cols, loading) = self.table_facts(cx);
        FooterFacts {
            ms: self.last_time_ms,
            count,
            cols,
            loading,
            page: self.page,
            page_size: self.page_size,
            total: self.total_rows,
            can_prev: self.page > 0,
            can_next: self.has_next(cx),
            can_last: matches!(self.last_page(), Some(lp) if self.page < lp),
            pageable: self.pageable,
        }
    }

    pub(crate) fn footer(&self, cx: &mut Context<Self>) -> Div {
        let t = pal(cx);
        let view = crate::prefs::get(cx).view;
        // The grid this footer describes. The Query view may have none
        // yet (no run, or a resultless statement) — then only the query
        // view's own status override speaks.
        let qgrid = (view == ViewMode::Query)
            .then(|| self.query_results_grid(cx))
            .flatten();
        let facts = match (view, &qgrid) {
            (ViewMode::Query, Some(g)) => Some(g.read(cx).footer_facts(cx)),
            (ViewMode::Query, None) => None,
            _ => Some(self.footer_facts(cx)),
        };
        // The Query view's transient voice — ticking "running…", a
        // note, or a resultless statement's "ok · N ms" — outranks the
        // grid stats while it has something to say.
        let qoverride = (view == ViewMode::Query)
            .then(|| {
                self.query_view.as_ref().and_then(|q| q.read(cx).status_override())
            })
            .flatten();
        let filter_open = self.filter_input.is_some();
        let rows_part = facts.as_ref().map(|f| {
            let base = f.page * f.page_size;
            let (first, last) = (base + 1, base + f.count);
            if f.count == 0 {
                "0 rows".to_string()
            } else {
                match f.total {
                    Some(t) => format!(
                        "{}\u{2013}{} of {} rows",
                        commas(first as u64),
                        commas(last as u64),
                        commas(t)
                    ),
                    None => format!(
                        "{}\u{2013}{} rows",
                        commas(first as u64),
                        commas(last as u64)
                    ),
                }
            }
        });
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
        let columns_part = facts.as_ref().map(|f| {
            format!("{} {}", f.cols, if f.cols == 1 { "column" } else { "columns" })
        });
        let verdict = facts.as_ref().map(|f| {
            format!(
                "{} \u{00b7} {}",
                crate::util::human(f.ms as f64 / 1000., "s"),
                rows_part.clone().unwrap_or_default()
            )
        });
        let loading_empty = view == ViewMode::Data
            && facts.as_ref().is_some_and(|f| f.loading && f.count == 0);
        let pager_visible = match view {
            ViewMode::Data => !loading_empty,
            // The pager holds its ground even while a run's ticking
            // override speaks (always-present chrome); it only stays
            // home for unpageable statements and empty scratchpads.
            ViewMode::Query => facts.as_ref().is_some_and(|f| f.pageable),
            ViewMode::Structure => false,
        };
        let status_prefix = match view {
            ViewMode::Data if loading_empty => Some("loading...".to_string()),
            ViewMode::Data => verdict,
            ViewMode::Query => qoverride.clone().or(verdict),
            ViewMode::Structure => None,
        };
        // Structure lists its columns with no prefix beside them; while
        // the query override speaks, the grid's column count stays quiet.
        let status_columns = match view {
            ViewMode::Structure => columns_part,
            ViewMode::Data if pager_visible => columns_part,
            ViewMode::Query if qoverride.is_none() => columns_part,
            _ => None,
        };
        let (can_prev, can_next, can_last, per) = facts
            .as_ref()
            .map(|f| (f.can_prev, f.can_next, f.can_last, f.page_size))
            .unwrap_or((false, false, false, 0));
        let dotted = status_prefix.is_some();
        div()
            .h_flex()
            .h(px(38.))
            .flex_none()
            .items_center()
            // Left inset matches the title strip and the grid text
            // (PANE_INSET), so the view switcher sits on the same axis as
            // everything above it.
            .pl(px(PANE_INSET))
            .pr(px(10.))
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
                        "view-structure",
                        "Structure",
                        view == ViewMode::Structure,
                        (true, false),
                        t,
                        cx.listener(|_, _, _, cx| {
                            crate::prefs::toggle(cx, |p| p.view = ViewMode::Structure);
                        }),
                    ))
                    .child(seg_sep(t))
                    // Structure, Data, Query — what it is, what it holds,
                    // what you ask (Sequel Pro's arc). Data, the default
                    // and hub, sits center: one ⌥-arrow from each side.
                    .child(seg_tile(
                        "view-data",
                        "Data",
                        view == ViewMode::Data,
                        (false, false),
                        t,
                        cx.listener(|_, _, _, cx| {
                            crate::prefs::toggle(cx, |p| p.view = ViewMode::Data);
                        }),
                    ))
                    .child(seg_sep(t))
                    .child(seg_tile(
                        "view-query",
                        "Query",
                        view == ViewMode::Query,
                        (false, true),
                        t,
                        cx.listener(|_, _, _, cx| {
                            crate::prefs::toggle(cx, |p| p.view = ViewMode::Query);
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
                // A breath between the funnel and the eye — the tiles
                // read as separate controls, not a fused cluster.
                d.child(div().ml_1().child(self.columns_popover(cx)))
            })
            // The staging story, told where the eye rests between edits:
            // the verb-split count while changes wait, "committing…"
            // while the transaction runs, or the read-only reason when
            // there is no primary key to key changes by (docs/EDITING.md
            // — a refusal is stated, never a mystery).
            .when(view == ViewMode::Data, |d| {
                let (updates, deletes) =
                    self.edits.as_ref().map(|e| e.counts()).unwrap_or((0, 0));
                if self.committing {
                    d.child(
                        div()
                            .ml_2()
                            .text_xs()
                            .text_color(t.muted)
                            .child("committing\u{2026}"),
                    )
                } else if updates + deletes > 0 {
                    d.child(div().ml_2().child(self.staged_popover(updates, deletes, cx)))
                } else if self.edits.is_none() && !loading_empty {
                    d.child(
                        div()
                            .ml_2()
                            .text_xs()
                            .text_color(t.muted.opacity(0.8))
                            .child("read-only \u{00b7} no primary key"),
                    )
                } else {
                    d
                }
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
                        d.when(dotted, |d| d.child(div().child("\u{00b7}")))
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
                                            this.pager_dispatch(cx, |g, cx| g.jump_first(cx))
                                        })),
                                    )
                                    .child(
                                        arrow(
                                            "page-prev",
                                            "icons/chevron-left.svg",
                                            can_prev,
                                        )
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.pager_dispatch(cx, |g, cx| g.prev_page(cx))
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
                                            .child(format!("{} per", commas(per as u64)))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.pager_dispatch(cx, |g, cx| {
                                                    g.cycle_page_size(cx)
                                                });
                                            })),
                                    )
                                    .child(
                                        arrow(
                                            "page-next",
                                            "icons/chevron-right.svg",
                                            can_next,
                                        )
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.pager_dispatch(cx, |g, cx| g.next_page(cx))
                                        })),
                                    )
                                    .child(
                                        arrow(
                                            "page-last",
                                            "icons/chevron-last.svg",
                                            can_last,
                                        )
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.pager_dispatch(cx, |g, cx| g.jump_last(cx))
                                        })),
                                    ),
                            )
                    }),
            )
    }

    /// The staged-changes chip and its review popover: the count is the
    /// trigger, the audit is pull-based (docs/EDITING.md). Each entry
    /// lists its diffs (`column: old → new`) with a per-entry discard;
    /// Commit and Discard all sit at the bottom.
    fn staged_popover(
        &self,
        updates: usize,
        deletes: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let grid = cx.entity();
        let t = pal(cx);
        let plural = |n: usize, word: &str| {
            if n == 1 {
                format!("1 {word}")
            } else {
                format!("{n} {word}s")
            }
        };
        // Verb-split label: updates in accent, deletes in the danger
        // color — destruction never hides inside a neutral count.
        let mut label = div().h_flex().items_center().gap_1().text_xs();
        if updates > 0 {
            label = label.child(div().text_color(t.accent).child(plural(updates, "update")));
        }
        if updates > 0 && deletes > 0 {
            label = label.child(div().text_color(t.muted).child("\u{00b7}"));
        }
        if deletes > 0 {
            label = label.child(div().text_color(t.bad).child(plural(deletes, "delete")));
        }
        label = label
            .child(div().text_color(t.muted).child("\u{00b7} \u{2318}S to commit"));
        gpui_component::popover::Popover::new("staged-popover")
            .anchor(Corner::BottomLeft)
            .trigger(
                gpui_component::button::Button::new("staged-btn")
                    .ghost()
                    .xsmall()
                    .child(label),
            )
            .content(move |_, _, cx| {
                let t = pal(cx);
                // Snapshot the entries: (key, row title, diff lines, is_delete).
                let items: Vec<(String, String, Vec<String>, bool)> = {
                    let g = grid.read(cx);
                    match &g.edits {
                        None => Vec::new(),
                        Some(e) => e
                            .entries()
                            .iter()
                            .map(|(key, identity, change)| {
                                let id = identity
                                    .iter()
                                    .map(vtext)
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                match change {
                                    crate::edits::RowChange::Delete => (
                                        key.to_string(),
                                        format!("row ({id})"),
                                        vec!["delete".to_string()],
                                        true,
                                    ),
                                    crate::edits::RowChange::Update(cells) => (
                                        key.to_string(),
                                        format!("row ({id})"),
                                        cells
                                            .iter()
                                            .map(|(col, cell)| {
                                                format!(
                                                    "{}: {} \u{2192} {}",
                                                    e.column_name(*col),
                                                    cell.original
                                                        .as_ref()
                                                        .map(|s| s.as_ref())
                                                        .unwrap_or("NULL"),
                                                    cell.text
                                                        .as_ref()
                                                        .map(|s| s.as_ref())
                                                        .unwrap_or("NULL"),
                                                )
                                            })
                                            .collect(),
                                        false,
                                    ),
                                }
                            })
                            .collect(),
                    }
                };
                let mut rows = div()
                    .id("staged-list")
                    .v_flex()
                    .p(px(4.))
                    .gap_px()
                    .max_h(px(340.))
                    .overflow_y_scroll();
                for (ix, (key, title, lines, is_delete)) in items.into_iter().enumerate() {
                    let grid = grid.clone();
                    rows = rows.child(
                        div()
                            .h_flex()
                            .items_start()
                            .gap_2()
                            .px(px(6.))
                            .py(px(4.))
                            .rounded(px(5.))
                            .hover(|d| d.bg(t.row_hover))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .v_flex()
                                    .gap_0p5()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(t.muted)
                                            .truncate()
                                            .child(title),
                                    )
                                    .children(lines.into_iter().map(|line| {
                                        div()
                                            .text_xs()
                                            .font_family(crate::theme::value_font())
                                            .text_color(if is_delete { t.bad } else { t.text })
                                            .truncate()
                                            .child(line)
                                    })),
                            )
                            .child(
                                div()
                                    .id(("staged-discard", ix))
                                    .flex_none()
                                    .px(px(4.))
                                    .rounded(px(4.))
                                    .cursor_pointer()
                                    .text_xs()
                                    .text_color(t.muted)
                                    .hover(|d| d.bg(t.row_hover).text_color(t.bad))
                                    .tooltip(|window, cx| {
                                        Tooltip::new("Discard this change").build(window, cx)
                                    })
                                    .child("\u{2715}")
                                    .on_click(move |_, _, cx| {
                                        grid.update(cx, |g, cx| {
                                            g.discard_change(&key, cx);
                                        });
                                    }),
                            ),
                    );
                }
                let discard_all = {
                    let grid = grid.clone();
                    div()
                        .id("staged-discard-all")
                        .text_xs()
                        .text_color(t.muted)
                        .cursor_pointer()
                        .hover(|d| d.text_color(t.bad))
                        .child("Discard all")
                        .on_click(move |_, _, cx| {
                            grid.update(cx, |g, cx| g.discard_all(cx));
                        })
                };
                let commit = {
                    let grid = grid.clone();
                    gpui_component::button::Button::new("staged-commit")
                        .primary()
                        .xsmall()
                        .label("Commit (\u{2318}S)")
                        .on_click(move |_, _, cx| {
                            grid.update(cx, |g, cx| g.commit(cx));
                        })
                };
                div()
                    .v_flex()
                    .w(px(320.))
                    .child(
                        div()
                            .h_flex()
                            .items_center()
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
                                    .child("STAGED CHANGES"),
                            )
                            .child(div().flex_1())
                            .child(discard_all),
                    )
                    .child(rows)
                    .child(
                        div()
                            .h_flex()
                            .justify_end()
                            .px(px(10.))
                            .py(px(8.))
                            .border_t_1()
                            .border_color(t.border)
                            .child(commit),
                    )
                    .into_any_element()
            })
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
