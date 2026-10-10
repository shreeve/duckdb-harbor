//! The content pane's bottom bar (DESIGN.md "Bottom bar", design.css
//! `.bbar`): the view switcher, the Data view's own controls (filter toggle,
//! Columns popover, Add Row and the staging story), and the right-anchored
//! status line and pager. The app renders it under whichever surface shows
//! (content.rs), and it describes that surface's grid: the table's, or the
//! Query view's results grid. The Data view's controls are an `impl Grid`
//! satellite, the same shape as `inspector.rs` and `structure.rs`.

use crate::app::DuckTable;
use crate::chrome::{icon_tile, seg_sep, seg_tile};
use crate::grid::Grid;
use crate::prefs::ViewMode;
use crate::theme::{pal, PANE_INSET};
use crate::util::commas;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use gpui_kit::component::button::ButtonVariants as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Disableable as _, Sizable as _, StyledExt as _};

/// A key value as the review popover shows it: strings bare, everything
/// else in its JSON spelling.
fn vtext(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// What the footer's right-anchored cluster renders from, read off the grid
/// the view shows.
#[derive(Clone, Debug, Default)]
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

/// What the status line shows: the leading text (time and row range, a
/// loading line, or the Query view's own word), the column count, and
/// whether the pager is there.
#[derive(Debug, PartialEq)]
pub(crate) struct Status {
    pub(crate) prefix: Option<String>,
    pub(crate) columns: Option<String>,
    pub(crate) pager: bool,
}

/// The status line for `view`, from its grid's facts (None when it has no
/// grid: no table chosen, or no result yet) and the Query view's own word
/// (`QueryView::status_override`: a ticking run, a note, a resultless
/// statement's "ok"), which outranks the grid's stats while it speaks.
pub(crate) fn status(view: ViewMode, facts: Option<&FooterFacts>, says: Option<String>) -> Status {
    let rows = |f: &FooterFacts| {
        let base = f.page * f.page_size;
        let (first, last) = (commas(base as u64 + 1), commas((base + f.count) as u64));
        match (f.count, f.total) {
            (0, _) => "0 rows".to_string(),
            (_, Some(total)) => format!("{first}\u{2013}{last} of {} rows", commas(total)),
            (_, None) => format!("{first}\u{2013}{last} rows"),
        }
    };
    let verdict = facts.map(|f| format!("{} \u{00b7} {}", crate::util::human(f.ms as f64 / 1000., "s"), rows(f)));
    let columns = facts.map(|f| format!("{} {}", f.cols, if f.cols == 1 { "column" } else { "columns" }));
    let loading = view == ViewMode::Data && facts.is_some_and(|f| f.loading && f.count == 0);
    // The pager holds its ground in the Query view even while a run's
    // ticking word speaks (always-present chrome); it stays home only for
    // a statement that cannot page, and with no result.
    let pager = match view {
        ViewMode::Data => facts.is_some() && !loading,
        ViewMode::Query => facts.is_some_and(|f| f.pageable),
        ViewMode::Structure => false,
    };
    let quiet = says.is_some();
    Status {
        prefix: match view {
            ViewMode::Data if loading => Some("loading...".to_string()),
            ViewMode::Data => verdict,
            ViewMode::Query => says.or(verdict),
            ViewMode::Structure => None,
        },
        // Structure lists its columns with nothing beside them; while the
        // Query view's word speaks, the grid's column count stays quiet.
        columns: match view {
            ViewMode::Structure => columns,
            ViewMode::Data if pager => columns,
            ViewMode::Query if !quiet => columns,
            _ => None,
        },
        pager,
    }
}

/// Why a table's grid takes no edits, said where the eye rests
/// (docs/EDITING.md: a refusal is stated, never a mystery). `pk_cols` is
/// the identity the grid asked for: the table's key, `rowid` for a table
/// without one, or nothing for a keyless table with a column of its own
/// named `rowid`, which hides DuckDB's.
pub(crate) fn read_only_reason(pk_cols: &[String]) -> &'static str {
    if pk_cols.is_empty() {
        "read-only \u{00b7} no key, and a column named rowid"
    } else {
        "read-only \u{00b7} its key is not among the columns read"
    }
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

    /// The Data view's controls beside the switcher: the filter toggle,
    /// the Columns popover, Add Row, and the staging story — the verb-split
    /// count while changes wait, "committing…" while the transaction runs,
    /// or why the table is read-only.
    pub(crate) fn data_controls(&self, cx: &mut Context<Self>) -> Div {
        let t = pal(cx);
        let (count, cols, loading) = self.table_facts(cx);
        let filter_open = self.filter_input.is_some();
        let (inserts, updates, deletes) = self.edits.as_ref().map(|e| e.counts()).unwrap_or((0, 0, 0));
        div()
            .h_flex()
            .items_center()
            // The filter toggle sits by the view switcher; accent when a
            // filter is ACTIVE, not just open.
            .child(
                icon_tile("toggle-filter", 22., true, t)
                    .ml_2()
                    .tooltip(|window, cx| Tooltip::new("Filter (raw SQL WHERE)").build(window, cx))
                    .child(
                        svg().path("icons/funnel.svg").size_3p5().text_color(
                            if self.filter.is_some() || filter_open { t.accent } else { t.muted },
                        ),
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_filter_strip(window, cx);
                    })),
            )
            // A breath between the funnel and the eye: the tiles read as
            // separate controls, not a fused cluster.
            .child(div().ml_1().child(self.columns_popover(cx)))
            .when(self.edits.is_some(), |d| {
                d.child(
                    gpui_kit::component::button::Button::new("add-row")
                        .icon(gpui_kit::component::IconName::Plus)
                        .ghost()
                        .xsmall()
                        .disabled(self.committing)
                        .tooltip("Add row")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.add_row(window, cx);
                        })),
                )
            })
            .map(|d| {
                let note = |text: &'static str| div().ml_2().text_xs().text_color(t.muted).child(text);
                if self.committing {
                    d.child(note("committing\u{2026}"))
                } else if inserts + updates + deletes > 0 {
                    d.child(div().ml_2().child(self.staged_popover(inserts, updates, deletes, cx)))
                } else if self.edits.is_none() && cols > 0 && !(loading && count == 0) {
                    d.child(note(read_only_reason(&self.pk_cols)).text_color(t.muted.opacity(0.8)))
                } else {
                    d
                }
            })
    }
}

impl DuckTable {
    /// The bottom bar under the content pane.
    pub(crate) fn footer(&self, cx: &mut Context<Self>) -> Div {
        let t = pal(cx);
        let view = crate::prefs::get(cx).view;
        let query = self.query.as_ref().filter(|_| view == ViewMode::Query).map(|q| q.read(cx));
        let shown = match &query {
            Some(query) => query.results_grid(),
            None => self.grid.clone().filter(|_| view != ViewMode::Query),
        };
        let says = query.and_then(|q| q.status_override());
        let facts = shown.as_ref().map(|g| g.read(cx).footer_facts(cx));
        let status = status(view, facts.as_ref(), says);
        let facts = facts.unwrap_or_default();
        let controls = match (&self.grid, view) {
            (Some(grid), ViewMode::Data) => Some(grid.update(cx, |g, cx| g.data_controls(cx))),
            _ => None,
        };
        let dotted = status.prefix.is_some();
        // Each segment lands on its view as ⌘1/⌘2/⌘3 do (`go_view`), focus
        // and all.
        let segment = |id, label, mode, ends| {
            seg_tile(id, label, view == mode, ends, t, move |_, _, cx| crate::go_view(mode, cx))
        };
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
            // The pane's bottom frame line, in the grid-line slot like
            // every line that frames the data surface.
            .border_color(t.grid_line)
            .child(
                // design.css `.seg`: the active fill runs flush to the
                // track's edges. gpui does not clip child backgrounds to
                // the track's radius, so each end segment carries its own
                // matching outer corners (nested radius = track radius -
                // border). Structure, Data, Query: what it is, what it
                // holds, what you ask (Sequel Pro's arc). Data, the default
                // and hub, sits center.
                div()
                    .h_flex()
                    .flex_none()
                    .rounded(px(8.))
                    .bg(t.surface)
                    .border_1()
                    .border_color(t.border)
                    .child(segment("view-structure", "Structure", ViewMode::Structure, (true, false)))
                    .child(seg_sep(t))
                    .child(segment("view-data", "Data", ViewMode::Data, (false, false)))
                    .child(seg_sep(t))
                    .child(segment("view-query", "Query", ViewMode::Query, (false, true))),
            )
            .children(controls)
            .child(div().flex_1())
            .child(
                // One right-anchored line: ms · range · columns · pager.
                // In a right-justified cluster an element moves only when
                // something to its RIGHT changes width, so the pager, the
                // only interactive element, is rightmost: constant-width
                // glyphs pinned to the corner, so neither a page flip nor
                // a table switch moves a click target ("N per" grows only
                // when the user cycles it). The column count is its OWN
                // node: as a suffix of a longer string its glyphs land a
                // subpixel differently, and a view switch shows a 1px
                // shift.
                div()
                    .ml_2()
                    .h_flex()
                    .flex_none()
                    .items_center()
                    .gap_2()
                    .text_xs()
                    .text_color(t.muted)
                    .when_some(status.prefix, |d, text| d.child(div().child(text)))
                    .when_some(status.columns, |d, text| {
                        d.when(dotted, |d| d.child(div().child("\u{00b7}"))).child(div().child(text))
                    })
                    .when_some(shown.filter(|_| status.pager), |d, grid| {
                        let arrow = |id: &'static str,
                                     path: &'static str,
                                     enabled: bool,
                                     act: fn(&mut Grid, &mut Context<Grid>)| {
                            let grid = grid.clone();
                            icon_tile(id, 20., enabled, t)
                                .text_color(if enabled { t.text } else { t.muted.opacity(0.4) })
                                .child(gpui_kit::component::Icon::empty().path(path).size_4())
                                .on_click(move |_, _, cx| grid.update(cx, act))
                        };
                        d.child(div().child("\u{00b7}")).child(
                            div()
                                .h_flex()
                                .items_center()
                                .gap_0p5()
                                .child(arrow("page-first", "icons/chevron-first.svg", facts.can_prev, Grid::jump_first))
                                .child(arrow("page-prev", "icons/chevron-left.svg", facts.can_prev, Grid::prev_page))
                                .child({
                                    let grid = grid.clone();
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
                                            Tooltip::new("Rows per page \u{2014} click to change").build(window, cx)
                                        })
                                        .child(format!("{} per", commas(facts.page_size as u64)))
                                        .on_click(move |_, _, cx| grid.update(cx, Grid::cycle_page_size))
                                })
                                .child(arrow("page-next", "icons/chevron-right.svg", facts.can_next, Grid::next_page))
                                .child(arrow("page-last", "icons/chevron-last.svg", facts.can_last, Grid::jump_last)),
                        )
                    }),
            )
    }
}

impl Grid {
    /// The staged-changes chip and its review popover: the count is the
    /// trigger, the audit is pull-based (docs/EDITING.md). Each entry
    /// lists its diffs (`column: old → new`) with a per-entry discard;
    /// Commit and Discard all sit at the bottom.
    fn staged_popover(
        &self,
        inserts: usize,
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
        // Verb-split label: inserts/updates in accent, deletes in the danger
        // color — destruction never hides inside a neutral count.
        let mut label = div().h_flex().items_center().gap_1().text_xs();
        if inserts > 0 {
            label = label.child(div().text_color(t.accent).child(plural(inserts, "insert")));
        }
        if inserts > 0 && (updates > 0 || deletes > 0) {
            label = label.child(div().text_color(t.muted).child("\u{00b7}"));
        }
        if updates > 0 {
            label = label.child(div().text_color(t.accent).child(plural(updates, "update")));
        }
        if updates > 0 && deletes > 0 {
            label = label.child(div().text_color(t.muted).child("\u{00b7}"));
        }
        if deletes > 0 {
            label = label.child(div().text_color(t.bad).child(plural(deletes, "delete")));
        }
        // A set held after a commit that got no answer is not sent by ⌘S:
        // the count says so, and opens the review that is the way out.
        let held = self.in_doubt();
        label = label.child(if held {
            div().text_color(t.bad).child("\u{00b7} held: may have landed \u{00b7} review")
        } else {
            div().text_color(t.muted).child("\u{00b7} \u{2318}S to commit")
        });
        gpui_kit::component::popover::Popover::new("staged-popover")
            .anchor(Anchor::BottomLeft)
            .trigger(
                gpui_kit::component::button::Button::new("staged-btn")
                    .ghost()
                    .xsmall()
                    .child(label),
            )
            .content(move |_, _, cx| {
                let t = pal(cx);
                let held = grid.read(cx).in_doubt();
                // Held against columns the table does not have, the set
                // cannot be staged again: one way out is left.
                let reshaped = held && grid.read(cx).is_reshaped();
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
                                    // A duplicate says which row it copies.
                                    crate::edits::RowChange::Insert(cells) => (
                                        key.to_string(),
                                        match e.source_label(identity) {
                                            Some(copy) => format!("new row \u{00b7} {copy}"),
                                            None => "new row".to_string(),
                                        },
                                        if cells.is_empty() {
                                            vec!["all columns: DEFAULT".to_string()]
                                        } else {
                                            cells
                                                .iter()
                                                .map(|(col, cell)| {
                                                    format!(
                                                        "{}: {}",
                                                        e.column_name(*col),
                                                        cell
                                                            .text
                                                            .as_ref()
                                                            .map(|s| s.as_ref())
                                                            .unwrap_or("NULL"),
                                                    )
                                                })
                                                .collect()
                                        },
                                        false,
                                    ),
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
                            // A held set gives up no single change: its
                            // commit landed whole or not at all.
                            .when(!held, |d| d.child(
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
                            )),
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
                // A held set is not committed from here. Its commit was all
                // or nothing, so it leaves the hold by one of two verdicts on
                // the whole set, and by nothing else.
                let footer = if held {
                    let verdict = |id: &'static str, label: &'static str, landed: bool| {
                        let grid = grid.clone();
                        gpui_kit::component::button::Button::new(id)
                            .xsmall()
                            .label(label)
                            .on_click(move |_, _, cx| {
                                grid.update(cx, |g, cx| g.judge_held(landed, cx));
                            })
                    };
                    div()
                        .h_flex()
                        .justify_end()
                        .gap_2()
                        .when(reshaped, |d| d.child(verdict("held-drop", "Discard all", true).danger()))
                        .when(!reshaped, |d| {
                            d.child(verdict("held-landed", "It landed: discard all", true).danger()).child(
                                verdict("held-not-landed", "It did not land: stage again", false).primary(),
                            )
                        })
                } else {
                    let grid = grid.clone();
                    div().h_flex().justify_end().child(
                        gpui_kit::component::button::Button::new("staged-commit")
                            .primary()
                            .xsmall()
                            .label("Commit (\u{2318}S)")
                            .on_click(move |_, _, cx| {
                                grid.update(cx, |g, cx| g.commit(cx));
                            }),
                    )
                };
                div()
                    .v_flex()
                    .w(px(if held { 380. } else { 320. }))
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
                                    .child(if held { "HELD CHANGES" } else { "STAGED CHANGES" }),
                            )
                            .child(div().flex_1())
                            .when(!held, |d| d.child(discard_all)),
                    )
                    .when(held, |d| {
                        d.child(
                            div()
                                .px(px(10.))
                                .py(px(6.))
                                .border_b_1()
                                .border_color(t.border)
                                .text_xs()
                                .text_color(t.bad)
                                .child(if reshaped {
                                    "The commit that sent these got no answer, and the table\u{2019}s \
                                     columns have changed since, so they cannot be staged again. \
                                     Whether the commit landed or not, dropping all of them is \
                                     the way out, and the table then loads as it is. The Query \
                                     tab shows what the table holds."
                                } else {
                                    "The commit that sent these got no answer. A commit is all \
                                     or nothing: either every change below is in the database, \
                                     or none is. They are off the page, which shows the \
                                     database. Compare, then say which. If it landed, all of \
                                     them are dropped, for good. If it did not, all of them go \
                                     back on the page, and \u{2318}S sends them."
                                }),
                        )
                    })
                    .child(rows)
                    .child(
                        div()
                            .px(px(10.))
                            .py(px(8.))
                            .border_t_1()
                            .border_color(t.border)
                            .child(footer),
                    )
                    .into_any_element()
            })
    }

    /// Column show/hide, in a popover that stays open across toggles.
    fn columns_popover(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let grid = cx.entity();
        gpui_kit::component::popover::Popover::new("columns-popover")
            .anchor(Anchor::BottomLeft)
            .trigger(
                gpui_kit::component::button::Button::new("columns-btn")
                    .icon(gpui_kit::component::IconName::Eye)
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
                                gpui_kit::component::checkbox::Checkbox::new(("col", ix))
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
                                gpui_kit::component::input::Input::new(&search)
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

#[cfg(test)]
mod tests {
    // Not `use super::*`: the glob would bring in gpui's `test` attribute.
    use super::{read_only_reason, status, FooterFacts, Status};
    use crate::prefs::ViewMode;

    fn page(count: usize, total: Option<u64>) -> FooterFacts {
        FooterFacts { ms: 1, count, cols: 9, page_size: 500, total, pageable: true, ..Default::default() }
    }

    #[test]
    fn the_status_line_describes_the_grid_the_view_shows() {
        let rows = page(500, Some(5_410));
        assert_eq!(
            status(ViewMode::Data, Some(&rows), None),
            Status {
                prefix: Some("1ms \u{b7} 1\u{2013}500 of 5,410 rows".into()),
                columns: Some("9 columns".into()),
                pager: true,
            }
        );
        // A first page on its way says so, and nothing else.
        let loading = FooterFacts { loading: true, ..page(0, None) };
        assert_eq!(
            status(ViewMode::Data, Some(&loading), None),
            Status { prefix: Some("loading...".into()), columns: None, pager: false }
        );
        // No table chosen, or no result yet: the switcher alone.
        let empty = Status { prefix: None, columns: None, pager: false };
        assert_eq!(status(ViewMode::Data, None, None), empty);
        assert_eq!(status(ViewMode::Query, None, None), empty);
        // Structure lists the columns alone.
        assert_eq!(
            status(ViewMode::Structure, Some(&rows), None),
            Status { prefix: None, columns: Some("9 columns".into()), pager: false }
        );
        // The Query view's word outranks the result's stats and quiets its
        // column count; the pager holds its ground for a result that pages.
        let said = status(ViewMode::Query, Some(&rows), Some("running\u{2026} 1.2s".into()));
        assert_eq!(said, Status { prefix: Some("running\u{2026} 1.2s".into()), columns: None, pager: true });
        let whole = FooterFacts { pageable: false, ..page(3, Some(3)) };
        assert!(!status(ViewMode::Query, Some(&whole), None).pager);
        assert_eq!(status(ViewMode::Query, None, Some("ok \u{b7} 2ms".into())).prefix.as_deref(), Some("ok \u{b7} 2ms"));
        assert_eq!(status(ViewMode::Data, Some(&page(0, Some(0))), None).prefix.as_deref(), Some("1ms \u{b7} 0 rows"));
    }

    #[test]
    fn a_read_only_table_says_why() {
        assert_eq!(read_only_reason(&[]), "read-only \u{b7} no key, and a column named rowid");
        assert_eq!(read_only_reason(&["id".to_string()]), "read-only \u{b7} its key is not among the columns read");
    }
}
