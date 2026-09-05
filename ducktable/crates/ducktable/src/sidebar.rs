//! The sidebar: berth rows with live-state dots, and the catalog tree.

use crate::app::{DuckTable, Phase};
use crate::chrome::head_glyph;
use crate::theme::{pal, Pal};
use crate::util::clone_str;
use crate::{AttachBerth, DetachBerth, SetTheme, StartBerth, StopBerth, ToggleAutostart};
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::button::{Button, ButtonCustomVariant, ButtonVariants as _};
use gpui_component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_component::*;
use harbor_client::Level;

/// The theme picker's menu: every bundled theme, grouped under its mode
/// and check-marked on the active one. A mode with no themes prints no
/// heading, so the grouping survives a theme set that is all one mode.
fn theme_menu(menu: PopupMenu, _: &mut Window, cx: &mut Context<PopupMenu>) -> PopupMenu {
    let themes = crate::theme::list(cx);
    let current = crate::theme::current_index(cx);
    let mut menu = menu;
    let mut first = true;
    for (dark, heading) in [(false, "LIGHT"), (true, "DARK")] {
        let group = themes.iter().enumerate().filter(|(_, (_, d))| *d == dark);
        for (n, (ix, (name, _))) in group.enumerate() {
            if n == 0 {
                if !first {
                    menu = menu.separator();
                }
                first = false;
                // BOLD CAPS a step smaller than the items: some themes'
                // muted color sits too close to the item color for a
                // plain label to read as a section header.
                menu = menu.item(
                    PopupMenuItem::element(move |_, _| {
                        div().text_xs().font_bold().child(heading)
                    })
                    .disabled(true),
                );
            }
            menu = menu.menu_with_check(name.clone(), ix == current, Box::new(SetTheme { ix }));
        }
    }
    menu
}

impl DuckTable {
    pub(crate) fn dot(level: Level, t: Pal) -> Div {
        div().size_2().rounded_full().bg(t.level(level))
    }

    /// The dot's stand-in while a berth is shutting down: the same disc's
    /// worth of space, filled by a slowly rotating refresh glyph in the
    /// row's live color, faded in so the swap from dot is not a hard cut.
    /// Reuses the embedded refresh-cw.svg — the Spinner component defaults
    /// to a Loader icon this app does not embed, which would draw blank.
    /// `seed` (the berth name) keys the animations so simultaneous stops
    /// never share an element id.
    fn spin_dot(seed: &str, level: Level, t: Pal) -> impl IntoElement {
        let spin = svg()
            .path("icons/refresh-cw.svg")
            .size_2()
            .text_color(t.level(level))
            .with_animation(
                SharedString::from(format!("berth-spin-{seed}")),
                Animation::new(std::time::Duration::from_millis(800)).repeat(),
                |el, delta| el.with_transformation(Transformation::rotate(percentage(delta))),
            );
        // In-place micro-fade — one shared duration with the copy tile's
        // crossfade (chrome::QUICK_FADE_MS), so the app's small fades match.
        div().child(spin).with_animation(
            SharedString::from(format!("berth-spinfade-{seed}")),
            Animation::new(std::time::Duration::from_millis(crate::chrome::QUICK_FADE_MS)),
            |el, delta| el.opacity(delta),
        )
    }

    pub(crate) fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
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
        // Width belongs to the resizable panel around this (main.rs
        // "root-split"); the sidebar just fills what it's granted.
        // No border of its own: the resize handle's 1px line IS the
        // divider (it also glows while dragging) — with both, the seam
        // read as 2px.
        div()
            .size_full()
            .bg(t.bg_sidebar)
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
                                .tooltip(move |window, cx| {
                                    gpui_component::tooltip::Tooltip::new("Filter databases")
                                        .build(window, cx)
                                })
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.toggle_berth_filter(window, cx);
                                })),
                        )
                    })
                    // The upgrade badge: a red count of local servers running
                    // an older harbor than the one installed. Click to restart
                    // them onto the current binary. Refresh (below) keeps its
                    // plain meaning; upgrading is this deliberate, separate tap.
                    .when(self.outdated_count() > 0, |d| {
                        let n = self.outdated_count();
                        d.child(
                            div()
                                .id("upgrade-badge")
                                .flex()
                                .items_center()
                                .justify_center()
                                .min_w(px(16.))
                                .h(px(16.))
                                .px_1()
                                .rounded_full()
                                .bg(t.bad)
                                .text_color(gpui::white())
                                .text_xs()
                                .font_weight(FontWeight::BOLD)
                                .cursor_pointer()
                                .child(n.to_string())
                                .tooltip(move |window, cx| {
                                    let what = if n == 1 {
                                        "1 database on an old harbor — click to upgrade".to_string()
                                    } else {
                                        format!("{n} databases on an old harbor — click to upgrade")
                                    };
                                    gpui_component::tooltip::Tooltip::new(what).build(window, cx)
                                })
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.prompt_upgrade(window, cx);
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
                            .tooltip(move |window, cx| {
                                gpui_component::tooltip::Tooltip::new("Refresh databases")
                                    .build(window, cx)
                            })
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.refresh(cx);
                            })),
                    ),
            )
            .when_some(self.berth_filter.clone(), |d, input| d.child(filter_row(&input)))
            // A config the loader refused or a refresh that failed would
            // otherwise blank or freeze this list silently — the failures
            // a GUI must say out loud.
            .when_some(self.warning.clone(), |d, warning| {
                d.child(
                    div()
                        .px_2()
                        .py_1()
                        .text_xs()
                        .text_color(t.warn)
                        .child(warning),
                )
            })
            .when(
                berth_filter.is_some()
                    && !self.rows.iter().any(|row| matches(&row.name, &berth_filter)),
                |d| d.child(empty_note(t, "No matching databases")),
            )
            .children(self.rows.iter().filter(|row| matches(&row.name, &berth_filter)).map(
                |row| {
                    let name = clone_str(&row.name);
                    let selected = active.as_deref() == Some(row.name.as_str());
                    let stopping = self.stopping.contains(&row.name);
                    let leaving = self.leaving.contains(&row.name);
                    let base = list_row(SharedString::from(clone_str(&row.name)), selected, t)
                        .px_2()
                        .child(if stopping {
                            Self::spin_dot(&row.name, row.state.level(), t).into_any_element()
                        } else {
                            Self::dot(row.state.level(), t).into_any_element()
                        })
                        // Same grammar as the table rows below: name with
                        // its count hugging it, magnitude on the right.
                        // (The dot alone says stopped; "on demand" gave
                        // way to the size, known even for stopped files.)
                        .child(named_count(&row.name, row.tables, t))
                        .when_some(row.size, |d, s| {
                            d.child(dim(t, crate::util::human(s as f64, "B")))
                        })
                        .when_some(row.note.clone(), |d, note| {
                            // a survey note rides the row as a tooltip: an
                            // unusual dot explains itself.
                            d.tooltip(move |window, cx| {
                                gpui_component::tooltip::Tooltip::new(clone_str(&note))
                                    .build(window, cx)
                            })
                        });
                    if leaving {
                        // Departed: hold the slot and fade out, no clicks —
                        // the fade timer in stop_berth drops the row once
                        // this animation has played. A whole row LEAVING is a
                        // structural motion, so it runs a touch longer than an
                        // in-place micro-fade (QUICK_FADE_MS) on purpose — the
                        // eye needs to track the departure (docs/UI.md, Motion).
                        return base
                            .with_animation(
                                SharedString::from(format!("berth-leaving-{}", row.name)),
                                Animation::new(std::time::Duration::from_millis(220)),
                                |el, delta| el.opacity(1.0 - delta),
                            )
                            .into_any_element();
                    }
                    if stopping {
                        // Shutting down: the spinner already says so, and a
                        // click mid-stop would only race the departure.
                        return base.into_any_element();
                    }
                    base
                        .on_click(cx.listener({
                            let name = clone_str(&name);
                            move |this, _: &ClickEvent, _, cx| this.connect(clone_str(&name), cx)
                        }))
                        // Right-click → Stop: shut this berth's server down.
                        // Disabled (greyed) for a stopped row — nothing to
                        // stop — so the menu is honest at a glance. stop()
                        // itself is idempotent, so the guard is UX, not
                        // safety. `stopped` is read here, not in the 'static
                        // menu closure, which may capture only owned data.
                        .context_menu({
                            let name = clone_str(&name);
                            let running = !matches!(row.state.level(), Level::Idle);
                            let attached = row.attached;
                            let autostart = row.autostart;
                            let path =
                                row.path.as_ref().map(|p| p.to_string_lossy().into_owned());
                            move |menu, _, _| {
                                // A remote has no local server lifecycle. Its
                                // one operation forgets the saved connection;
                                // dropping the active Conn also closes SSH.
                                let Some(path) = path.clone() else {
                                    return menu.menu(
                                        "Remove Database",
                                        Box::new(crate::RemoveRemoteDatabase {
                                            name: clone_str(&name),
                                        }),
                                    );
                                };
                                // Running axis: exactly one of Start / Stop.
                                let menu = if running {
                                    menu.menu("Stop", Box::new(StopBerth { name: clone_str(&name) }))
                                } else {
                                    menu.menu("Start", Box::new(StartBerth { path: path.clone() }))
                                };
                                // Membership axis: exactly one of Attach / Detach.
                                let menu = menu.separator();
                                let menu = if attached {
                                    menu.menu("Detach", Box::new(DetachBerth { path: path.clone() }))
                                } else {
                                    menu.menu("Attach", Box::new(AttachBerth { path: path.clone() }))
                                };
                                // Autostart: one checkmark item that flips the
                                // login item (check on the left, by default).
                                menu.menu_with_check(
                                    "Autostart",
                                    autostart,
                                    Box::new(ToggleAutostart { path, on: !autostart }),
                                )
                            }
                        })
                        .into_any_element()
                },
            ))
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
                    // The theme picker leads the footer: the active
                    // theme's name, opening the full list on one click.
                    // Anchored BottomLeft so it grows up off the footer
                    // rather than off the bottom of the window.
                    .child(
                        Button::new("theme")
                            .label(crate::theme::current_name(cx))
                            .custom(
                                ButtonCustomVariant::new(cx)
                                    .foreground(t.muted)
                                    .hover(t.row_hover),
                            )
                            .compact()
                            .xsmall()
                            .dropdown_menu_with_anchor(Corner::BottomLeft, theme_menu),
                    )
                    .child(div().flex_1()),
            )
    }

    fn catalog_tree(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let t = pal(cx);
        let family_sort = crate::prefs::get(cx).family_sort;
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
        let table_total = catalog.tables.len();
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
                // Family sort: base tables ahead of their compound
                // children (`orders` above `order_items`) — see
                // family_cmp. Accent-lit while on, like the filter.
                .child(
                    head_glyph("family-sort", family_sort, t)
                        .child(svg().path("icons/shapes.svg").size_3p5().text_color(
                            if family_sort { t.accent } else { t.muted },
                        ))
                        .tooltip(move |window, cx| {
                            gpui_component::tooltip::Tooltip::new("Sort objects")
                                .build(window, cx)
                        })
                        .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                            crate::prefs::toggle(cx, |p| p.family_sort = !p.family_sort);
                        })),
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
                            .tooltip(move |window, cx| {
                                gpui_component::tooltip::Tooltip::new("Filter tables")
                                    .build(window, cx)
                            })
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
                        .tooltip(move |window, cx| {
                            gpui_component::tooltip::Tooltip::new("Refresh Tables (⌘R)")
                                .build(window, cx)
                        })
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.refresh_catalog(cx);
                        })),
                ),
        );
        if let Some(input) = &self.table_filter {
            tree = tree.child(filter_row(input));
        }
        if filter.is_some()
            && !catalog.tables.iter().any(|table| matches(&table.name, &filter))
        {
            tree = tree.child(empty_note(t, "No matching tables"));
        }
        for schema in schemas {
            if many_schemas {
                tree = tree.child(
                    div().px_2().py_1().text_xs().text_color(t.muted).child(clone_str(schema)),
                );
            }
            for table in tables_in_order(catalog, schema, family_sort) {
                if !matches(&table.name, &filter) {
                    continue;
                }
                let key = (clone_str(schema), clone_str(&table.name));
                let selected = self.selected_table.as_ref() == Some(&key);
                tree = tree.child(
                    list_row(
                        SharedString::from(format!("t-{schema}-{}", table.name)),
                        selected,
                        t,
                    )
                    // Text starts on the database names' axis: their
                    // rows indent 8px pad + 8px dot + 8px gap = 24.
                    .pl(px(24.))
                    .pr_2()
                    // "users (35)" — the column count hugs the name; a
                    // long name truncates but keeps its count visible.
                    .child(named_count(&table.name, Some(table.columns.len()), t))
                    // Exact row count in compact SI form — rows are what a
                    // scan of a database cares about; column counts
                    // live in the footer status and Structure view.
                    .when_some(table.row_count, |d, n| {
                        d.child(dim(t, crate::util::human(n as f64, "")))
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

impl crate::app::DuckTable {
    /// The sidebar's table list, in the exact order and under the exact
    /// filter the tree renders — ⌥←/⌥→ (App::step_table) walks THIS
    /// list, so arrow order always agrees with what the eye sees.
    pub(crate) fn visible_tables(&self, cx: &App) -> Vec<(String, String)> {
        let crate::app::Phase::Connected { catalog, .. } = &self.phase else {
            return Vec::new();
        };
        let filter = self
            .table_filter
            .as_ref()
            .map(|i| i.read(cx).value().to_string().to_lowercase())
            .filter(|s| !s.is_empty());
        let family_sort = crate::prefs::get(cx).family_sort;
        let mut out = Vec::new();
        for schema in catalog.schemas() {
            for table in tables_in_order(catalog, schema, family_sort) {
                if matches(&table.name, &filter) {
                    out.push((schema.to_string(), table.name.clone()));
                }
            }
        }
        out
    }
}

/// The catalog's tables for one schema, in the sidebar's chosen order.
/// Both renderers of that order (the tree and visible_tables) come
/// through here, so the keyboard can never disagree with the eye.
fn tables_in_order<'a>(
    catalog: &'a harbor_client::Catalog,
    schema: &str,
    family_sort: bool,
) -> Vec<&'a harbor_client::catalog::Table> {
    let mut tables = catalog.tables_in(schema);
    if family_sort {
        tables.sort_by(|a, b| family_cmp(&a.name, &b.name));
    }
    tables
}

/// Family collation: `_` sorts after the alphabet instead of before it,
/// so a base table leads its compound children — `orders` above
/// `order_items`, `partners` above `partner_emails` — which plain
/// bytewise order inverts (`_` = 0x5F < any lowercase letter). No
/// pluralization smarts: making the separator heavy is the whole rule.
fn family_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    fn key(s: &str) -> impl Iterator<Item = u8> + '_ {
        s.bytes().map(|c| if c == b'_' { 0xFF } else { c })
    }
    key(a).cmp(key(b))
}

/// The one filter test both sidebar lists apply, spelled once.
fn matches(name: &str, filter: &Option<String>) -> bool {
    match filter {
        Some(f) => name.to_lowercase().contains(f.as_str()),
        None => true,
    }
}

/// A sidebar list row's chassis — hover, selection tint, click target.
/// Callers add their inset and children.
fn list_row(id: SharedString, selected: bool, t: Pal) -> Stateful<Div> {
    div()
        .id(id)
        .py_1()
        .rounded_md()
        .h_flex()
        .gap_2()
        .items_center()
        .cursor_pointer()
        .when(selected, |d| d.bg(t.row_selected))
        .hover(|d| d.bg(t.row_hover))
}

/// "name (n)" — the count hugs the name, so a long name truncates but
/// keeps its count visible; the row's right side stays free for a
/// magnitude.
fn named_count(name: &str, count: Option<usize>, t: Pal) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .h_flex()
        .gap_1()
        .items_center()
        .child(div().min_w_0().truncate().text_sm().text_color(t.text).child(clone_str(name)))
        .when_some(count, |d, n| {
            d.child(div().flex_none().text_xs().text_color(t.muted).child(format!("({n})")))
        })
}

/// A row's right-side magnitude (size on disk, exact row count), muted.
fn dim(t: Pal, text: String) -> Div {
    div().text_xs().text_color(t.muted).child(text)
}

/// The open filter's input row, shared by both lists.
fn filter_row(input: &Entity<gpui_component::input::InputState>) -> Div {
    div().px_2().pb_1().child(gpui_component::input::Input::new(input).xsmall().cleanable(true))
}

/// What a filter says when it filtered everything away — without this a
/// too-narrow query just blanks the list.
fn empty_note(t: Pal, text: &'static str) -> Div {
    div().px_2().py_1().text_xs().text_color(t.muted).child(text)
}
