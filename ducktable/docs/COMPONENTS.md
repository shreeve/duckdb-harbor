# Component inventory: gpui-component 0.5.1

What the UI spec needs versus what gpui-component ships, from a read of
the 0.5.1 source (2026-08-29). Re-check this file on every
gpui-component upgrade; it describes a pre-1.0 crate and will drift.

Everything below is consumed directly from view code against the PINNED
versions; an upgrade is a deliberate review-everything event. Widgets
that fight us get replaced by first-party drawing at the call site (the
grid's selection painting and cell borders are the pattern), not wrapped.

## Direct fits

| Spec component | Module | Notes |
| --- | --- | --- |
| Data grid (display) | `table/` | Delegate-based, virtualized, `sortable`, `col_resizable`, loading states |
| SQL editor | `input/` | `CodeEditor` mode (tab size, indent); `input/lsp/` has diagnostics, completion, hover |
| Catalog tree | `tree.rs` | Virtualized via `uniform_list` |
| Three-pane layout | `dock/`, `resizable/` | `DockArea`, stack/tab panels, split handles, layout state persistence |
| Tab strip | `tab/`, dock `tab_panel` | |
| Notebook panes | `collapsible.rs`, `accordion.rs`, `resizable/`, `v_virtual_list` | Composition; no new primitives needed |
| Theming | `theme/` | JSON theme files, semantic tokens, `ThemeRegistry`, `watch_dir` hot reload. Our five themes are one ThemeSet file (`assets/themes/ducktable.json`) |
| Platform titlebar | `title_bar.rs` | Per-platform caption handling |
| Chrome | `menu`, `popover`, `dialog`, `breadcrumb`, `badge`, `spinner`, `skeleton`, `notification`, `kbd` | |
| Chart pane (later) | `chart/`, `plot/` | Exists; untouched until the notebook chart phase |

## Gaps we own

1. **Grid editing layer.** The Table renders and sorts; inline editing,
   the shared cell/editor geometry, dirty marks, conflict cells, and
   the staged/live pipeline are ours, built on the delegate API. This
   is the app's core engineering and is governed by DESIGN.md's rules.
2. **SQL syntax highlighting.** The tree-sitter highlighter has a
   language registry but bundles no SQL grammar. We register
   `tree-sitter-sql`; the bundled `languages/` directory is the
   template. Completion itself is Harbor-side (`sql_auto_complete`).
3. **Berth status, LIVE badge, NULL tags, conflict cells.** Small
   custom drawing inside our wrappers.

## Measure before trusting

**Column-axis virtualization: verified, 2026-08-29.** Both axes
virtualize — rows through `uniform_list`, columns through
`virtual_list`, with `render_td` called only for visible cells
(gpui-component 0.5.1 `table/state.rs`). The probe lives at
`crates/ducktable/examples/wide_probe.rs`: a self-driving 500-column x
100,000-row table that sweeps six scroll patterns and prints frame-time
stats. Re-run it on every gpui-component bump:

```
cargo run --release -p ducktable --example wide_probe
```

Measured on an M-series Mac at a 60 Hz refresh, release build: every
phase pinned at the 16.7 ms vsync interval — p50 16.7 ms, p95 no worse
than 17.6 ms, and no frame over 33 ms in any scroll phase, including
random jumps on both axes at once (full viewport replacement per
frame). RSS 128.9 MB with no row data stored, which is the
framework-plus-window baseline. Verdict: the Table is the grid's
display and scroll foundation; no column-windowing wrapper is needed.
