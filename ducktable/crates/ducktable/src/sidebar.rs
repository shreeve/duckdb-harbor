//! The sidebar: berth rows with live-state dots, and the catalog tree.

use crate::app::{DuckTable, Phase};
use crate::sql::Rel;
use crate::chrome::head_glyph;
use crate::theme::{pal, Pal};
use crate::util::clone_str;
use crate::{AttachBerth, DetachBerth, SetTheme, StartBerth, StopBerth, ToggleAutostart};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::*;
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
    /// `seed` (the row's element id) keys the animations so simultaneous
    /// stops never share an element id.
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
                                    gpui_kit::component::Icon::new(
                                        gpui_kit::component::IconName::Search,
                                    )
                                    .size_3p5(),
                                )
                                .tooltip(move |window, cx| {
                                    gpui_kit::component::tooltip::Tooltip::new("Filter databases")
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
                    .when(self.outdated().next().is_some(), |d| {
                        let n = self.outdated().count();
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
                                .text_color(gpui_kit::white())
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
                                    gpui_kit::component::tooltip::Tooltip::new(what).build(window, cx)
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
                                gpui_kit::component::tooltip::Tooltip::new("Refresh databases")
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
                    // The clicked berth highlights the moment it is clicked
                    // (the in-flight one wins over the still-rendering old
                    // connection), and of two rows that share a name only
                    // the one that is the database lights up.
                    let selected = self.is_active_row(row);
                    // A Stop in flight belongs to one database: another
                    // row of the same name neither spins nor fades with it.
                    let stopping = self.stopping.contains(&row.key);
                    let leaving = self.leaving.contains(&row.key);
                    // Names can repeat, across two files and across a file
                    // and a remote, and an element id cannot.
                    let id = row.key.element_id();
                    let base = list_row(SharedString::from(clone_str(&id)), selected, t)
                        .px_2()
                        .child(if stopping {
                            Self::spin_dot(&id, row.state.level(), t).into_any_element()
                        } else {
                            Self::dot(row.state.level(), t).into_any_element()
                        })
                        // Same grammar as the table rows below: name with
                        // its count hugging it, magnitude on the right.
                        // The dot alone says stopped, and the size shows
                        // whether it runs or not.
                        .child(named_count(if row.workspace { "Workspace" } else { &row.name }, row.tables, t))
                        .when_some(row.size, |d, s| {
                            d.child(dim(t, crate::util::human(s as f64, "B")))
                        })
                        .when_some(row.note.clone(), |d, note| {
                            // a survey note rides the row as a tooltip: an
                            // unusual dot explains itself.
                            d.tooltip(move |window, cx| {
                                gpui_kit::component::tooltip::Tooltip::new(clone_str(&note))
                                    .build(window, cx)
                            })
                        });
                    if leaving {
                        // Departed: hold the slot and fade out, no clicks —
                        // the fade timer in stop_berth drops the row once
                        // this animation has played. A whole row LEAVING is a
                        // structural motion, so it runs a touch longer than an
                        // in-place micro-fade (QUICK_FADE_MS) on purpose — the
                        // eye needs to track the departure (docs/DESIGN.md, Motion).
                        return base
                            .with_animation(
                                SharedString::from(format!("berth-leaving-{id}")),
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
                        // The row connects to what it shows: its file, or
                        // the remote of its name. Never the name looked up
                        // again, which a file and a remote can share.
                        .on_click(cx.listener({
                            let name = clone_str(&name);
                            let path = row.path.clone();
                            move |this, _: &ClickEvent, _, cx| {
                                this.choose_row(clone_str(&name), path.clone(), cx)
                            }
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
                            let path = row.path.clone();
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
                                    menu.menu(
                                        "Stop",
                                        Box::new(StopBerth {
                                            name: clone_str(&name),
                                            path: path.clone(),
                                        }),
                                    )
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
                            .dropdown_menu_with_anchor(Anchor::BottomLeft, theme_menu),
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
        let filter = self
            .table_filter
            .as_ref()
            .map(|i| i.read(cx).value().to_string().to_lowercase())
            .filter(|s| !s.is_empty());
        let filter_open = self.table_filter.is_some();
        let table_total = catalog.tables.len()
            + catalog.views.len()
            + catalog.attached.iter().map(|a| a.tables.len() + a.views.len()).sum::<usize>();
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
                            gpui_kit::component::tooltip::Tooltip::new("Sort objects")
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
                                gpui_kit::component::Icon::new(gpui_kit::component::IconName::Search)
                                    .size_3p5(),
                            )
                            .tooltip(move |window, cx| {
                                gpui_kit::component::tooltip::Tooltip::new("Filter tables")
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
                            gpui_kit::component::tooltip::Tooltip::new("Refresh Tables (⌘R)")
                                .build(window, cx)
                        })
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.refresh_tables(cx);
                        })),
                ),
        );
        if let Some(input) = &self.table_filter {
            tree = tree.child(filter_row(input));
        }
        let lines = tree_lines(catalog, family_sort, &filter);
        if filter.is_some() && !lines.iter().any(|line| line.rel().is_some()) {
            tree = tree.child(empty_note(t, "No matching tables"));
        }
        let workspace = self.on_workspace();
        for line in lines {
            tree = tree.child(self.tree_line(line, workspace, t, cx));
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
    pub(crate) fn visible_tables(&self, cx: &App) -> Vec<Rel> {
        let crate::app::Phase::Connected { catalog, .. } = &self.phase else {
            return Vec::new();
        };
        let filter = self
            .table_filter
            .as_ref()
            .map(|i| i.read(cx).value().to_string().to_lowercase())
            .filter(|s| !s.is_empty());
        let family_sort = crate::prefs::get(cx).family_sort;
        tree_lines(catalog, family_sort, &filter).into_iter().filter_map(|line| line.rel().cloned()).collect()
    }

    /// Whether the database on screen is the workspace, whose views read
    /// files and can be imported or removed.
    pub(crate) fn on_workspace(&self) -> bool {
        let connected = self.connected_key();
        self.rows.iter().any(|r| r.workspace && Some(&r.key) == connected.as_ref())
    }

    /// One line of the TABLES tree, drawn.
    fn tree_line(&self, line: Line, workspace: bool, t: Pal, cx: &mut Context<Self>) -> AnyElement {
        match line {
            Line::Schema { name, nested } => div()
                .pl(px(if nested { 24. } else { 8. }))
                .py_1()
                .text_xs()
                .text_color(t.muted)
                .child(name)
                .into_any_element(),
            Line::Catalog { name, storage } => {
                let detach = clone_str(&name);
                div()
                    .id(SharedString::from(format!("catalog-{name}")))
                    .pl_2()
                    .pr_2()
                    .pt_2()
                    .pb_1()
                    .h_flex()
                    .gap_1()
                    .items_center()
                    .child(div().flex_1().min_w_0().truncate().text_xs().font_weight(FontWeight::BOLD).text_color(t.muted).child(name))
                    .child(dim(t, storage))
                    .context_menu(move |menu, _, _| {
                        menu.menu("Detach", Box::new(crate::DetachCatalog { name: clone_str(&detach) }))
                    })
                    .into_any_element()
            }
            Line::Table { rel, columns, rows } => {
                let selected = self.selected_table.as_ref() == Some(&rel);
                let menu_rel = rel.clone();
                self.relation_row(&rel, selected, t, cx)
                    .child(named_count(&rel.name, Some(columns), t))
                    // Exact row count in compact SI form — rows are what a
                    // scan of a database cares about; column counts
                    // live in the footer status and Structure view.
                    .when_some(rows, |d, n| d.child(dim(t, crate::util::human(n as f64, ""))))
                    .context_menu(move |menu, window, cx| save_as_menu(menu, &menu_rel, window, cx))
                    .into_any_element()
            }
            Line::View { rel, columns, error, file } => {
                let selected = self.selected_table.as_ref() == Some(&rel);
                let menu_rel = rel.clone();
                // A view in the workspace that reads a file can become a
                // table, and leave the workspace; any other view is the
                // database's own, and only saved.
                let own = workspace && rel.catalog.is_none() && file;
                let row = self.relation_row(&rel, selected, t, cx);
                let row = match &error {
                    // A view that does not read says why, and shows no
                    // columns it does not have.
                    Some(error) => {
                        let error = clone_str(error);
                        row.child(div().flex_1().min_w_0().truncate().text_sm().text_color(t.warn).child(clone_str(&rel.name)))
                            .tooltip(move |window, cx| {
                                gpui_kit::component::tooltip::Tooltip::new(clone_str(&error)).build(window, cx)
                            })
                    }
                    None => row.child(named_count(&rel.name, Some(columns), t)),
                };
                // Read-only, said where a table says its rows.
                row.child(dim(t, "view".to_string()))
                    .context_menu(move |menu, window, cx| {
                        let menu = match own {
                            true => menu.menu("Import as Table", Box::new(crate::ImportView { rel: menu_rel.clone() })),
                            false => menu,
                        };
                        let menu = save_as_menu(menu, &menu_rel, window, cx);
                        match own {
                            true => menu
                                .separator()
                                .menu("Remove from Workspace", Box::new(crate::RemoveView { rel: menu_rel.clone() })),
                            false => menu,
                        }
                    })
                    .into_any_element()
            }
        }
    }

    /// A table's or a view's row: selected on a click, and on a double
    /// click shown in the Data view.
    fn relation_row(&self, rel: &Rel, selected: bool, t: Pal, cx: &mut Context<Self>) -> Stateful<Div> {
        let key = rel.clone();
        let id = format!("t-{}-{}-{}", rel.catalog.as_deref().unwrap_or(""), rel.schema, rel.name);
        list_row(SharedString::from(id), selected, t)
            // Text starts on the database names' axis: their
            // rows indent 8px pad + 8px dot + 8px gap = 24.
            .pl(px(24.))
            .pr_2()
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if event.click_count() == 2 {
                    crate::prefs::toggle(cx, |prefs| {
                        prefs.view = crate::prefs::ViewMode::Data;
                    });
                }
                this.select_table(key.clone(), window, cx);
            }))
    }
}

/// Save As, a submenu of the four formats Export writes.
fn save_as_menu(menu: PopupMenu, rel: &Rel, window: &mut Window, cx: &mut Context<PopupMenu>) -> PopupMenu {
    let rel = rel.clone();
    menu.submenu("Save As", window, cx, move |menu, _, _| {
        use crate::export::Format;
        [("CSV\u{2026}", Format::Csv), ("Parquet\u{2026}", Format::Parquet), ("JSON\u{2026}", Format::Json), ("Excel\u{2026}", Format::Excel)]
            .into_iter()
            .fold(menu, |menu, (label, format)| menu.menu(label, Box::new(crate::SaveAs { rel: rel.clone(), format })))
    })
}

/// One line of the TABLES tree.
#[derive(Debug, PartialEq)]
pub(crate) enum Line {
    /// A schema's heading: among the database's own when it has more than
    /// one, or within an attached catalog that has more than one
    /// (`nested`).
    Schema { name: String, nested: bool },
    /// An attached catalog's heading, and its engine's name for its
    /// storage (`sqlite`).
    Catalog { name: String, storage: String },
    /// A table, its column count and its exact row count where the
    /// catalog has one.
    Table { rel: Rel, columns: usize, rows: Option<u64> },
    /// A view, read-only: its column count, why it does not read when it
    /// does not, and whether it reads a file (`workspace::view_source`).
    View { rel: Rel, columns: usize, error: Option<String>, file: bool },
}

impl Line {
    /// The relation the line selects, for a table or a view.
    pub(crate) fn rel(&self) -> Option<&Rel> {
        match self {
            Line::Table { rel, .. } | Line::View { rel, .. } => Some(rel),
            Line::Schema { .. } | Line::Catalog { .. } => None,
        }
    }
}

/// The TABLES tree, line by line: the database's own tables, then its
/// views, schema by schema with a heading when there is more than one
/// schema; then each attached catalog under a heading of its own, its
/// tables and views in the same order. The tree and ⌥←/⌥→
/// (`visible_tables`) both read it, so the keyboard walks what the eye
/// sees. A filter keeps the tables and views whose names hold it, and
/// every heading.
pub(crate) fn tree_lines(catalog: &harbor_client::Catalog, family_sort: bool, filter: &Option<String>) -> Vec<Line> {
    let mut out = Vec::new();
    let order = |a: &str, b: &str| if family_sort { family_cmp(a, b) } else { a.cmp(b) };
    let relations = |out: &mut Vec<Line>, catalog_name: Option<&str>, tables: Vec<&harbor_client::Table>, views: Vec<&wire::catalog::View>, schema: &str| {
        let rel = |name: &str| match catalog_name {
            Some(c) => Rel::of(c, schema, name),
            None => Rel::own(schema, name),
        };
        let mut tables = tables;
        tables.sort_by(|a, b| order(&a.name, &b.name));
        for t in tables.into_iter().filter(|t| matches(&t.name, filter)) {
            out.push(Line::Table { rel: rel(&t.name), columns: t.columns.len(), rows: t.row_count });
        }
        let mut views = views;
        views.sort_by(|a, b| order(&a.name, &b.name));
        for v in views.into_iter().filter(|v| matches(&v.name, filter)) {
            let file = v.ddl.as_deref().and_then(harbor_client::workspace::view_source).is_some();
            out.push(Line::View { rel: rel(&v.name), columns: v.columns.len(), error: v.error.clone(), file });
        }
    };
    let schemas = catalog.schemas();
    for schema in &schemas {
        if schemas.len() > 1 {
            out.push(Line::Schema { name: schema.to_string(), nested: false });
        }
        relations(&mut out, None, catalog.tables_in(schema), catalog.views_in(schema), schema);
    }
    for attached in &catalog.attached {
        out.push(Line::Catalog { name: attached.name.clone(), storage: attached.storage.clone() });
        let schemas = attached.schemas();
        for schema in &schemas {
            if schemas.len() > 1 {
                out.push(Line::Schema { name: schema.to_string(), nested: true });
            }
            relations(&mut out, Some(&attached.name), attached.tables_in(schema), attached.views_in(schema), schema);
        }
    }
    out
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
fn filter_row(input: &Entity<gpui_kit::component::input::InputState>) -> Div {
    div().px_2().pb_1().child(gpui_kit::component::input::Input::new(input).xsmall().cleanable(true))
}

/// What a filter says when it filtered everything away — without this a
/// too-narrow query just blanks the list.
fn empty_note(t: Pal, text: &'static str) -> Div {
    div().px_2().py_1().text_xs().text_color(t.muted).child(text)
}

#[cfg(test)]
mod tests {
    use super::{tree_lines, Line};
    use crate::sql::Rel;
    use wire::catalog::{Attached, Catalog, Table, View};

    fn table(schema: &str, name: &str) -> Table {
        Table { name: name.into(), schema: schema.into(), row_count: Some(3), ..Default::default() }
    }

    fn view(name: &str, ddl: &str, error: Option<&str>) -> View {
        View { name: name.into(), schema: "main".into(), ddl: Some(ddl.into()), error: error.map(Into::into), ..Default::default() }
    }

    fn rels(lines: &[Line]) -> Vec<Rel> {
        lines.iter().filter_map(|l| l.rel().cloned()).collect()
    }

    #[test]
    fn the_tree_lists_tables_then_views_then_each_attached_catalog() {
        let catalog = Catalog {
            tables: vec![table("main", "orders"), table("main", "order_items")],
            views: vec![
                view("sales", "CREATE VIEW sales AS SELECT * FROM read_csv('/x/sales.csv');", None),
                view("gone", "CREATE VIEW gone AS SELECT * FROM read_csv('/x/gone.csv');", Some("IO Error: No files found")),
                view("calc", "CREATE VIEW calc AS SELECT 1;", None),
            ],
            attached: vec![Attached {
                name: "app".into(),
                storage: "sqlite".into(),
                tables: vec![table("main", "people")],
                ..Default::default()
            }],
            ..Default::default()
        };
        let lines = tree_lines(&catalog, false, &None);
        assert_eq!(
            rels(&lines),
            [
                Rel::own("main", "order_items"),
                Rel::own("main", "orders"),
                Rel::own("main", "calc"),
                Rel::own("main", "gone"),
                Rel::own("main", "sales"),
                Rel::of("app", "main", "people"),
            ]
        );
        // A view says why it does not read, and whether it reads a file.
        assert!(lines.contains(&Line::View {
            rel: Rel::own("main", "gone"),
            columns: 0,
            error: Some("IO Error: No files found".into()),
            file: true
        }));
        assert!(lines.contains(&Line::View { rel: Rel::own("main", "calc"), columns: 0, error: None, file: false }));
        // The attached catalog has a heading of its own; one schema needs none.
        assert_eq!(lines[5], Line::Catalog { name: "app".into(), storage: "sqlite".into() });
        assert!(!lines.iter().any(|l| matches!(l, Line::Schema { .. })));
        // Family sort puts a base table ahead of its children.
        assert_eq!(rels(&tree_lines(&catalog, true, &None))[..2], [Rel::own("main", "orders"), Rel::own("main", "order_items")]);
        // A filter keeps what matches, tables and views alike, and the headings.
        let lines = tree_lines(&catalog, false, &Some("e".into()));
        assert_eq!(rels(&lines), [Rel::own("main", "order_items"), Rel::own("main", "orders"), Rel::own("main", "gone"), Rel::own("main", "sales"), Rel::of("app", "main", "people")]);
        let lines = tree_lines(&catalog, false, &Some("zzz".into()));
        assert!(rels(&lines).is_empty() && lines.len() == 1);
    }

    #[test]
    fn schemas_are_headed_only_where_there_is_more_than_one() {
        let catalog = Catalog {
            tables: vec![table("main", "a"), table("audit", "log")],
            attached: vec![Attached { name: "app".into(), tables: vec![table("main", "p"), table("x", "q")], ..Default::default() }],
            ..Default::default()
        };
        let lines = tree_lines(&catalog, false, &None);
        let heads: Vec<_> = lines.iter().filter(|l| l.rel().is_none()).collect();
        assert_eq!(
            heads,
            [
                &Line::Schema { name: "main".into(), nested: false },
                &Line::Schema { name: "audit".into(), nested: false },
                &Line::Catalog { name: "app".into(), storage: String::new() },
                &Line::Schema { name: "main".into(), nested: true },
                &Line::Schema { name: "x".into(), nested: true },
            ]
        );
        assert_eq!(rels(&lines)[3], Rel::of("app", "x", "q"));
    }
}
