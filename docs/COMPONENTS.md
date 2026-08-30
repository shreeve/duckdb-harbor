# Component inventory: gpui-component 0.5.1

What the UI spec needs versus what gpui-component ships, from a read of
the 0.5.1 source (2026-08-29). Re-check this file on every
gpui-component upgrade; it describes a pre-1.0 crate and will drift.

Everything below is consumed through our own wrapper modules, never
directly from view code, so an upstream break or a component swap stays
a contained diff.

## Direct fits

| Spec component | Module | Notes |
| --- | --- | --- |
| Data grid (display) | `table/` | Delegate-based, virtualized, `sortable`, `col_resizable`, loading states |
| SQL editor | `input/` | `CodeEditor` mode (tab size, indent); `input/lsp/` has diagnostics, completion, hover |
| Catalog tree | `tree.rs` | Virtualized via `uniform_list` |
| Three-pane layout | `dock/`, `resizable/` | `DockArea`, stack/tab panels, split handles, layout state persistence |
| Tab strip | `tab/`, dock `tab_panel` | |
| Notebook panes | `collapsible.rs`, `accordion.rs`, `resizable/`, `v_virtual_list` | Composition; no new primitives needed |
| Theming | `theme/` | JSON theme files, semantic tokens, `ThemeRegistry`, `watch_dir` hot reload. Our five themes are five JSON files |
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

**Column-axis virtualization is unverified.** The table is virtualized,
but whether in both axes is not established, and wide analytics results
are a stated scale target. The first task of the grid phase is an
empirical probe: render a 500-column x 100,000-row result and measure
frame time and memory before any editing work builds on top. If only
rows virtualize, our wrapper windows columns. A client that assumes
per-cell views scale has been wrong before; this repo does not assume.
