# Harbor's vendored reedline

This is reedline 0.50.0, vendored here and wired via `[patch.crates-io]` in
the workspace `Cargo.toml`, carrying four patches in `src/engine.rs`. Each is
covered by tests in this copy (`enter_with_an_empty_menu_submits_the_line`,
`typing_past_a_completion_then_enter_runs_the_line`,
`a_word_boundary_closes_the_menu`, `a_word_ends_inside_a_burst_of_typing`,
`statement_punctuation_defuses_an_always_suggesting_menu`,
`menu_accept_only_accepts_an_active_selection`,
`down_on_the_live_line_is_inapplicable`,
`down_walks_history_back_to_the_live_line_and_only_then_falls_through`,
`a_recalled_line_keeps_down_as_history_even_after_an_edit`,
`down_inside_a_multiline_buffer_moves_the_cursor`,
`a_new_line_after_a_recalled_one_is_live`):

- **Patch A** — a completion menu whose filtered suggestions are EMPTY does
  not swallow Enter: the Enter/Submit guard skips valueless menus so the
  event falls through to submit, and `submit_buffer` deactivates straggler
  menus. (The bug: Tab opened the menu, it stayed active for the whole rest
  of the line, and Enter was routed to it — inserting the highlighted word
  at end of line, or dying on an empty menu.)
- **Patch B** — typing a word boundary deactivates the menu, fish/zsh-style,
  so a stale menu can't linger to intercept a later Enter. Boundary = any
  char that can't extend a completable word (whitespace, `;`, `)`, quotes…);
  only word chars and `.` (qualified names) keep it. Whitespace-only proved
  insufficient in the field: after `;` DuckDB's grammar completer suggests
  next-statement keywords, so the menu was non-empty and Patch A couldn't
  save the Enter (`show tab` Tab `les;` Enter appended "table").

  Every `InsertChar` in the batch is examined, not just the first.
  `process_input_batch` fuses consecutive edits into one `ReedlineEvent::Edit`,
  so a burst of typing delivers `les;` as a single event and the boundary can
  sit anywhere in it.
- **Patch C** — `ReedlineEvent::MenuAccept` accepts an active menu's selection
  without submitting, so a binding can take a completion and leave Enter to
  the line. Reports `Inapplicable` when no menu is active or the active one
  has no values, so it composes under `UntilFound`.
- **Patch D** — `ReedlineEvent::Down` reports `Inapplicable` on the live
  line: the cursor on the buffer's last line, the buffer not recalled from
  history (or emptied since), and no traversal in progress. Upstream always
  reports it handled, even when it does nothing, so no binding could ever
  fall through Down to something else. A `buffer_from_history` flag is set
  when a history item is painted into the buffer, cleared when a walk that
  began on the live line lands back on it (a walk from an edited recalled
  line lands back on that line, which stays recalled) and at the start of
  every `read_line`, and ignored once the buffer is empty. A recalled line therefore keeps Up
  and Down as history even after an edit — the keys never change meaning
  under a user's hands — and Down opens harbor's completion panel only
  where history has nothing below.

Harbor registers one menu, `ReedlineMenu::EngineCompleter`
(`crates/harbor/src/repl/interactive.rs`). Patch B's boundary is therefore
engine-wide here without consequence; a menu that filters on whole command
lines, such as a history menu, would need the boundary scoped to the menu.

The 0.50.0 sources carry CRLF line endings and are not rustfmt-clean, both
inherited from the crates.io tarball. Leave them as they are — reformatting
buries the three patches in noise.

## Un-vendor when upstream lands

Upstream status (`gh pr view <n> --repo nushell/reedline`):

- **Patch A: merged** — https://github.com/nushell/reedline/pull/1175
- **Patch B: open** — https://github.com/nushell/reedline/pull/1209, tracking
  https://github.com/nushell/reedline/issues/1176. Upstream scopes the
  boundary to the menu via `MenuBuilder::with_word_chars(Some("_."))`, which
  harbor must call once un-vendored; the default keeps a menu open for the
  rest of the line.
- **Patch C: merged** — https://github.com/nushell/reedline/pull/1203, landed
  upstream independently of this copy.
- **Patch D: not filed.** It changes what `Down` reports, which upstream may
  see as a behavior change for every binding built on it; harbor's binding
  needs it, so it stays here until there is an upstream conversation.

Also open: https://github.com/nushell/reedline/pull/1210, collapsing the
duplicated menu-accept rule Patch A and Patch C each state separately.

crates.io is at 0.51.0, which predates all three, so the earliest release
carrying them is 0.52.

When every patch is upstream AND released (Patch D included, or re-applied
on top of the release): delete `reedline` from
`[patch.crates-io]` and the `exclude` list in `harbor/Cargo.toml`,
`rm -rf vendor/reedline`, bump the reedline version in
`crates/harbor/Cargo.toml`, add the `with_word_chars` call to the completion
menu, then `cargo test -p harbor && make test SUITES="unit types spec catalog
sessions cancel"` and re-run the repro: `create or replace ta`, Tab, keep
typing, Enter — the statement must run with no stray word appended. Type the
tail fast enough to arrive as one batch, or the boundary scan goes untested.

Known upstream flake, not ours: `cargo test --all-features` on macOS
segfaults intermittently in the system-clipboard tests (parallel pasteboard
access) — it does so on clean upstream main too; default-feature and
single-threaded runs are green.
