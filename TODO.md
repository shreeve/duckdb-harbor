# TODO

Ongoing maintenance items. Architecture rationale and shipped/unshipped
decisions live in [PLAN.md](PLAN.md).

## justhttp — harbor's first-party HTTP layer (no un-vendor plan; it's ours)

`crates/justhttp` is harbor's own synchronous HTTP/1.1 crate — a workspace
member and plain path dependency of `crates/harbor`, designed and maintained in
this repository (decision record: PLAN.md D12; historical lineage and license:
`crates/justhttp/README.md`). It is not vendored and has no upstream HTTP-crate
dependency or synchronization work. The default suite (`cargo test -p
justhttp`) is green on macOS and Linux;
`cargo test -p justhttp --test suite -- --ignored` separately runs the one slow
(~35s) write-timeout test.

Maintenance notes:

- **The two hardening behaviors have regression tests** — `tests/drain.rs`
  (a dropped request with `Content-Length: 1 GiB` must allocate < 1 MiB;
  measured via a global allocator) and the `stall` module in `tests/suite.rs` (a client that stops
  reading its response cannot pin a worker; the 10s write timeout frees it).
  If either test ever fails, that is a security regression, not a flake.
- **What justhttp deliberately lacks** (do not "fix"): TLS (edge proxy's job,
  D6), websocket upgrades, HTTP/2 and /3, `TestRequest`/test scaffolding, and
  every API harbor doesn't call. New surface should be added only when harbor
  needs it.

## Watch: vendored reedline — stray-word-on-Enter fixed, un-vendor when upstream lands

FIXED 2026-08-19 by vendoring reedline 0.50.0 at `vendor/reedline/` (wired via
`[patch.crates-io]`) with two patches in `src/engine.rs`, both covered by
tests in the vendored copy (`enter_with_an_empty_menu_submits_the_line`,
`a_word_boundary_closes_the_menu`,
`typing_past_a_completion_then_enter_runs_the_line`):

- **Patch A** — a completion menu whose filtered suggestions are EMPTY no
  longer swallows Enter: the Enter/Submit guard skips valueless menus so the
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

Upstream status (check with `gh pr view 1175 --repo nushell/reedline` and
`gh issue view 1176 --repo nushell/reedline`):

- **Patch A PR: https://github.com/nushell/reedline/pull/1175**
- **Patch B proposal: https://github.com/nushell/reedline/issues/1176**
  (offered as-is or behind an option; PR it when maintainers pick a shape)

When A is merged AND released (and B is either upstream or consciously
dropped): delete `reedline` from `[patch.crates-io]` and the `exclude` list
in the root Cargo.toml, `rm -rf vendor/reedline`, bump the reedline version
in `crates/pilot/Cargo.toml`, then `cargo test -p pilot && make check_quick`
and re-run the repro: `create or replace ta`, Tab, keep typing, Enter — the
statement must run with no stray word appended.

Known upstream flake, not ours: `cargo test --all-features` on macOS
segfaults intermittently in the system-clipboard tests (parallel pasteboard
access) — it does so on CLEAN main too; default-feature and single-threaded
runs are green.
