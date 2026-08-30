//! The sidebar: berth rows with live-state dots, and the catalog tree.

use crate::app::{DuckTable, Phase};
use crate::theme::{pal, Pal};
use crate::util::clone_str;
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::*;
use harbor_client::Level;

impl DuckTable {
    pub(crate) fn dot(level: Level, t: Pal) -> Div {
        div().size_2().rounded_full().bg(t.level(level))
    }

    pub(crate) fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = pal(cx);
        let berth_filter = self
            .berth_filter
            .as_ref()
            .map(|i| i.read(cx).value().to_string().to_lowercase())
            .filter(|s| !s.is_empty());
        // The clicked berth highlights the moment it is clicked (the
        // in-flight name wins over the still-rendering old connection).
        let active = match (&self.connecting, &self.phase) {
            (Some(name), _) => Some(clone_str(name)),
            (None, Phase::Connected { conn, .. }) => Some(clone_str(&conn.name)),
            _ => None,
        };
        div()
            .w_56()
            .flex_none()
            .h_full()
            .bg(t.bg_sidebar)
            .border_r_1()
            .border_color(t.border)
            .px_2()
            .pt_2()
            .v_flex()
            .gap_px()
            .child(
                div()
                    .pl_2()
                    .pr_1()
                    .py_1()
                    .h_flex()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .font_weight(FontWeight::BOLD)
                            .text_color(t.muted)
                            // "Berth" is Harbor's word; the UI says what a
                            // user sees. (Not "CONNECTIONS" — that reads
                            // as saved connection configs, which this
                            // list is not.)
                            .child("DATABASES"),
                    )
                    // A filter earns its glyph past 10 items (and stays
                    // while open, so it can always be closed).
                    .when(self.berth_filter.is_some() || self.rows.len() > 10, |d| {
                        d.child(
                            head_glyph("filter-berths", self.berth_filter.is_some(), t)
                                .child(
                                    gpui_component::Icon::new(
                                        gpui_component::IconName::Search,
                                    )
                                    .size_3p5(),
                                )
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.toggle_berth_filter(window, cx);
                                })),
                        )
                    })
                    .child(
                        head_glyph("refresh-berths", false, t)
                            .child(
                                svg()
                                    .path("icons/refresh-cw.svg")
                                    .size_3p5()
                                    .text_color(t.muted),
                            )
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.refresh(cx);
                            })),
                    ),
            )
            .when_some(self.berth_filter.clone(), |d, input| {
                d.child(
                    div()
                        .px_2()
                        .pb_1()
                        .child(gpui_component::input::Input::new(&input).xsmall().cleanable(true)),
                )
            })
            .children(self.rows.iter().filter(|row| {
                match &berth_filter {
                    Some(f) => row.name.to_lowercase().contains(f.as_str()),
                    None => true,
                }
            }).map(|row| {
                let name = clone_str(&row.name);
                let selected = active.as_deref() == Some(row.name.as_str());
                div()
                    .id(SharedString::from(clone_str(&row.name)))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .h_flex()
                    .gap_2()
                    .items_center()
                    .cursor_pointer()
                    .when(selected, |d| d.bg(t.row_selected))
                    .hover(|d| d.bg(t.row_hover))
                    .child(Self::dot(row.state.level(), t))
                    .child(
                        // Same grammar as the table rows below: name with
                        // its count hugging it, magnitude on the right.
                        // (The dot alone says stopped; "on demand" gave
                        // way to the size, known even for stopped files.)
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_flex()
                            .gap_1()
                            .items_center()
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_sm()
                                    .text_color(t.text)
                                    .child(clone_str(&row.name)),
                            )
                            .when_some(row.tables, |d, n| {
                                d.child(
                                    div()
                                        .flex_none()
                                        .text_xs()
                                        .text_color(t.muted)
                                        .child(format!("({n})")),
                                )
                            }),
                    )
                    .when_some(row.size, |d, s| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(t.muted)
                                .child(crate::util::human_bytes(s)),
                        )
                    })
                    .when_some(row.note.clone(), |d, note| {
                        // reconcile's fix-it line ("… harbor forget x")
                        // rides the row as a tooltip: the unhealthy dot
                        // explains itself.
                        d.tooltip(move |window, cx| {
                            gpui_component::tooltip::Tooltip::new(clone_str(&note))
                                .build(window, cx)
                        })
                    })
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.connect(clone_str(&name), cx);
                    }))
            }))
            .child(self.catalog_tree(cx))
            .child(
                div()
                    .mt_2()
                    // Same height as the grid footer, with no sidebar
                    // padding below: refresh and the theme name center on
                    // the same line as the footer's Data/Structure labels.
                    .h(px(38.))
                    // The grid footer's top border sits inside ITS 38px,
                    // centering its labels 1px lower; match it.
                    .pt(px(1.))
                    .flex_none()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    // The theme cycler leads the footer.
                    .child(
                        div()
                            .id("theme")
                            .px_2()
                            .py_1()
                            .text_xs()
                            .text_color(t.muted)
                            .cursor_pointer()
                            .hover(|d| d.text_color(t.accent))
                            .child(crate::theme::current_name(cx))
                            .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                                crate::theme::cycle(cx);
                            })),
                    )
                    .child(div().flex_1()),
            )
    }

    fn catalog_tree(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let t = pal(cx);
        let Phase::Connected { catalog, .. } = &self.phase else {
            return div().id("catalog").flex_1();
        };
        let schemas = catalog.schemas();
        let many_schemas = schemas.len() > 1;
        let filter = self
            .table_filter
            .as_ref()
            .map(|i| i.read(cx).value().to_string().to_lowercase())
            .filter(|s| !s.is_empty());
        let filter_open = self.table_filter.is_some();
        let table_total: usize =
            schemas.iter().map(|s| catalog.tables_in(s).len()).sum();
        let mut tree = div().id("catalog").flex_1().min_h_0().overflow_y_scroll().v_flex().gap_px();
        tree = tree.child(
            div()
                .pl_2()
                .pr_1()
                .pt_3()
                .pb_1()
                .h_flex()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .flex_1()
                        .text_xs()
                        .font_weight(FontWeight::BOLD)
                        .text_color(t.muted)
                        .child("TABLES"),
                )
                // A filter earns its glyph past 10 items (and stays
                // while open, so it can always be closed).
                .when(filter_open || table_total > 10, |d| {
                    d.child(
                        head_glyph("filter-tables", filter_open, t)
                            .child(
                                gpui_component::Icon::new(gpui_component::IconName::Search)
                                    .size_3p5(),
                            )
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.toggle_table_filter(window, cx);
                            })),
                    )
                })
                .child(
                    head_glyph("refresh-catalog", false, t)
                        // A raw svg() does NOT inherit text color (Icon
                        // sets it explicitly); without this it's invisible.
                        .child(svg().path("icons/refresh-cw.svg").size_3p5().text_color(t.muted))
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.refresh_catalog(cx);
                        })),
                ),
        );
        if let Some(input) = &self.table_filter {
            tree = tree.child(
                div()
                    .px_2()
                    .pb_1()
                    .child(gpui_component::input::Input::new(input).xsmall().cleanable(true)),
            );
        }
        for schema in schemas {
            if many_schemas {
                tree = tree.child(
                    div().px_2().py_1().text_xs().text_color(t.muted).child(clone_str(schema)),
                );
            }
            for table in catalog.tables_in(schema) {
                if let Some(f) = &filter {
                    if !table.name.to_lowercase().contains(f.as_str()) {
                        continue;
                    }
                }
                let key = (clone_str(schema), clone_str(&table.name));
                let selected = self.selected_table.as_ref() == Some(&key);
                tree = tree.child(
                    div()
                        .id(SharedString::from(format!("t-{schema}-{}", table.name)))
                        // Text starts on the database names' axis: their
                        // rows indent 8px pad + 8px dot + 8px gap = 24.
                        .pl(px(24.))
                        .pr_2()
                        .py_1()
                        .rounded_md()
                        .h_flex()
                        .gap_2()
                        .items_center()
                        .cursor_pointer()
                        .when(selected, |d| d.bg(t.row_selected))
                        .hover(|d| d.bg(t.row_hover))
                        .child(
                            // "users (35)" — the column count hugs the
                            // name; a long name truncates but keeps its
                            // count visible.
                            div()
                                .flex_1()
                                .min_w_0()
                                .h_flex()
                                .gap_1()
                                .items_center()
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .text_sm()
                                        .text_color(t.text)
                                        .child(clone_str(&table.name)),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .text_xs()
                                        .text_color(t.muted)
                                        .child(format!("({})", table.columns.len())),
                                ),
                        )
                        // Row count in compact SI form — rows are what a
                        // scan of a database cares about; column counts
                        // live in the footer status and Structure view.
                        .when_some(table.estimated_rows, |d, n| {
                            d.child(
                                div()
                                    .text_xs()
                                    .text_color(t.muted)
                                    .child(crate::util::human_count(n)),
                            )
                        })
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.select_table(
                                clone_str(&key.0),
                                clone_str(&key.1),
                                window,
                                cx,
                            );
                        })),
                );
            }
        }
        if !catalog.sequences.is_empty() {
            tree = tree.child(
                div()
                    .px_2()
                    .pt_2()
                    .pb_1()
                    .text_xs()
                    .font_weight(FontWeight::BOLD)
                    .text_color(t.muted)
                    .child("SEQUENCES"),
            );
            for seq in &catalog.sequences {
                tree = tree.child(
                    div()
                        .pl(px(24.))
                        .py_1()
                        .text_sm()
                        .text_color(t.muted)
                        .child(clone_str(&seq.name)),
                );
            }
        }
        tree
    }
}

/// A small icon button for a sidebar section header (filter, refresh).
fn head_glyph(id: &'static str, on: bool, t: Pal) -> Stateful<Div> {
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
