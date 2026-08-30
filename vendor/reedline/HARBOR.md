# Harbor's vendored reedline

This is reedline 0.50.0, vendored here and wired via `[patch.crates-io]` in
the workspace `Cargo.toml`, carrying two patches in `src/engine.rs`. Both are
covered by tests in this copy (`enter_with_an_empty_menu_submits_the_line`,
`a_word_boundary_closes_the_menu`,
`typing_past_a_completion_then_enter_runs_the_line`):

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

## Un-vendor when upstream lands

Upstream status (check with `gh pr view 1175 --repo nushell/reedline` and
`gh issue view 1176 --repo nushell/reedline`):

- **Patch A PR: https://github.com/nushell/reedline/pull/1175**
- **Patch B proposal: https://github.com/nushell/reedline/issues/1176**
  (offered as-is or behind an option; PR it when maintainers pick a shape)

When A is merged AND released (and B is either upstream or consciously
dropped): delete `reedline` from `[patch.crates-io]` and the `exclude` list
in the root Cargo.toml, `rm -rf vendor/reedline`, bump the reedline version
in `crates/pilot/Cargo.toml`, then `cargo test -p pilot && make test
SUITES="unit types spec catalog sessions cancel"` and re-run the repro:
`create or replace ta`, Tab, keep typing, Enter — the statement must run
with no stray word appended.

Known upstream flake, not ours: `cargo test --all-features` on macOS
segfaults intermittently in the system-clipboard tests (parallel pasteboard
access) — it does so on clean upstream main too; default-feature and
single-threaded runs are green.
