# TODO

Ongoing maintenance items. Not the roadmap — that's [PLAN.md](PLAN.md).

## Watch: vendored tiny_http — TWO patches, un-vendor carefully

We carry a **vendored tiny_http** at `vendor/tiny_http/` (wired in via
`[patch.crates-io]` in the root `Cargo.toml`) with **two independent harbor
patches**. Only the first is filed upstream — so un-vendoring naively would
silently drop the second. Track them separately.

### Patch 1 — unauthenticated memory-exhaustion DoS (filed upstream)

`EqualReader::drop` drained an unread request body with
`vec![0; declared_content_length]`, so a request declaring a huge
`Content-Length` while sending a few bytes cost the process that whole
allocation at drop time. Measured before the fix: RSS 22 MB → 2.2 GB under
a handful of concurrent probes. See the note in
`vendor/tiny_http/src/util/equal_reader.rs`.

- **PR: https://github.com/tiny-http/tiny-http/pull/290**

```sh
gh pr view 290 --repo tiny-http/tiny-http --json state,mergedAt
```

### Patch 2 — response write timeout (NOT yet upstreamed)

`Connection::set_write_timeout` (`src/connection.rs`) plus a
`WRITE_TIMEOUT` (10s) applied to every accepted socket in the accept loop
(`src/lib.rs`). Fixes "B": a client that stops reading its response otherwise
pins a worker thread inside `write` forever (tiny_http sets no write timeout).
Read side is deliberately untouched so keep-alive idle still waits. This is a
small, general-purpose addition — **it should be upstreamed too** (file a PR
for `set_write_timeout` on the accept path), so that un-vendoring restores the
backstop for free instead of losing it. Until then, if you un-vendor you MUST
re-apply Patch 2 or accept losing the stalled-reader protection.

### When Patch 1 is merged AND released — un-vendor (only if Patch 2 is handled)

1. Confirm Patch 2 is either upstreamed (in the same release) or you have a
   plan to re-apply it — otherwise do NOT un-vendor yet.
2. Delete the `[patch.crates-io]` block from the root `Cargo.toml`.
3. Bump the `tiny_http` version in `crates/harbor/Cargo.toml` to the
   release that contains the fix(es).
4. `rm -rf vendor/tiny_http` (and `vendor/` if nothing else lives there).
5. `make binary && make check` — all 10 suites must pass.
6. Re-run the DoS check (concurrent `POST /sql` with
   `Content-Length: 1000000000` sending a few bytes must leave RSS flat) AND
   the stalled-reader check (a client that reads headers then stops must have
   its worker reclaimed, not pinned forever).

Until then the vendored copy is correct and harbor is safe; this is cleanup,
not a fix.

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
- **Patch B** — typing a word boundary (whitespace) deactivates the menu,
  fish/zsh-style, so a stale menu can't linger to intercept a later Enter.

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
