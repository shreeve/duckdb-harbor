//! The sidebar: berth rows with live-state dots, and the catalog tree.

use crate::app::{DuckTable, Phase};
use crate::theme::*;
use crate::util::clone_str;
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::*;
use harbor_client::Level;

impl DuckTable {
    pub(crate) fn dot(level: Level) -> Div {
        let color = match level {
            Level::Good => rgb(GOOD),
            Level::Warn => rgb(WARN),
            Level::Bad => rgb(BAD),
            Level::Idle => rgb(MUTED),
        };
        div().size_2().rounded_full().bg(color)
    }

    pub(crate) fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = match &self.phase {
            Phase::Connected { conn, .. } => Some(clone_str(&conn.name)),
            Phase::Connecting { name } => Some(clone_str(name)),
            _ => None,
        };
        div()
            .w_56()
            .flex_none()
            .h_full()
            .bg(rgb(BG_SIDEBAR))
            .border_r_1()
            .border_color(rgb(BORDER))
            .p_2()
            .v_flex()
            .gap_px()
            .child(
                div()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .font_weight(FontWeight::BOLD)
                    .text_color(rgb(MUTED))
                    .child("BERTHS"),
            )
            .children(self.rows.iter().map(|row| {
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
                    .when(selected, |d| d.bg(rgb(0xD6E6FB)))
                    .hover(|d| d.bg(rgb(0xE4EDF8)))
                    .child(Self::dot(row.state.level()))
                    .child(div().flex_1().text_sm().text_color(rgb(TEXT)).child(clone_str(&row.name)))
                    .when(row.summonable, |d| {
                        d.child(div().text_xs().text_color(rgb(MUTED)).child("spawn"))
                    })
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.connect(clone_str(&name), cx);
                    }))
            }))
            .child(self.catalog_tree(cx))
            .child(
                div()
                    .id("refresh")
                    .mt_2()
                    .px_2()
                    .py_1()
                    .flex_none()
                    .text_xs()
                    .text_color(rgb(ACCENT))
                    .cursor_pointer()
                    .child("refresh")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.refresh(cx))),
            )
    }

    pub(crate) fn catalog_tree(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let Phase::Connected { catalog, .. } = &self.phase else {
            return div().id("catalog").flex_1();
        };
        let schemas = catalog.schemas();
        let many_schemas = schemas.len() > 1;
        let mut tree = div().id("catalog").flex_1().min_h_0().overflow_y_scroll().v_flex().gap_px();
        tree = tree.child(
            div()
                .px_2()
                .pt_3()
                .pb_1()
                .text_xs()
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(MUTED))
                .child("CATALOG"),
        );
        for schema in schemas {
            if many_schemas {
                tree = tree.child(
                    div().px_2().py_1().text_xs().text_color(rgb(MUTED)).child(clone_str(schema)),
                );
            }
            for table in catalog.tables_in(schema) {
                let key = (clone_str(schema), clone_str(&table.name));
                let selected = self.selected_table.as_ref() == Some(&key);
                tree = tree.child(
                    div()
                        .id(SharedString::from(format!("t-{schema}-{}", table.name)))
                        .pl_4()
                        .pr_2()
                        .py_1()
                        .rounded_md()
                        .h_flex()
                        .gap_2()
                        .items_center()
                        .cursor_pointer()
                        .when(selected, |d| d.bg(rgb(0xD6E6FB)))
                        .hover(|d| d.bg(rgb(0xE4EDF8)))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_sm()
                                .text_color(rgb(TEXT))
                                .child(clone_str(&table.name)),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(MUTED))
                                .child(format!("{}", table.columns.len())),
                        )
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.selected_table = Some((clone_str(&key.0), clone_str(&key.1)));
                            cx.notify();
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
                    .text_color(rgb(MUTED))
                    .child("SEQUENCES"),
            );
            for seq in &catalog.sequences {
                tree = tree.child(
                    div()
                        .pl_4()
                        .py_1()
                        .text_sm()
                        .text_color(rgb(MUTED))
                        .child(clone_str(&seq.name)),
                );
            }
        }
        tree
    }
}
