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

## Fix: pilot autocomplete inserts a stray word on Enter (diagnosed, unfixed)

The bug: sometimes accepting/ignoring completion leaves an extra word (e.g.
`table`) appended at the end of the line. Fully diagnosed 2026-08-18 — it is
NOT in pilot's completer (`crates/pilot/src/complete.rs` is span-correct); it
is a reedline 0.50.0 design flaw pilot inherits through the standard menu
wiring in `crates/pilot/src/repl.rs::make_editor`:

1. Tab opens the completion menu — and reedline keeps it ACTIVE for the whole
   rest of the line. It refilters on every keystroke but only closes on Esc,
   Enter, or an empty buffer (`engine.rs` Edit arm, ~line 1692: no
   deactivate-on-empty, no deactivate-on-word-boundary).
2. Any Enter while any menu is active is CONSUMED to insert the highlighted
   suggestion at the cursor instead of submitting (`engine.rs` ~line 1575,
   the `Enter | Submit | SubmitOrNewline if menu.is_active()` arm). Cursor is
   at end of line → stray word appended.
3. Sub-case: menu refiltered to zero matches still eats the Enter and inserts
   nothing — the "had to press Enter twice" ghost.

The complete fix cannot be made from pilot's side (acceptance is hardwired to
Enter; menus expose no deactivate hook to the completer). Options weighed:

- **(1) Vendor reedline + two-line patch** (the tiny_http precedent).
  Patch A: an active menu with ZERO suggestions must not eat Enter — fall
  through to submit. Patch B: typing a word boundary (whitespace, `;`)
  deactivates the menu, fish/zsh-style. Together the menu can only intercept
  Enter while visibly showing matches for the word under the cursor —
  standard shell behavior. Cost: ~15k-line crate vendored for two lines.
- **(2) Pilot-side mitigation only**: return no server-lane suggestions when
  the cursor is not on a word (cache lane already does). Shrinks the window,
  cannot close it — a statement ending in a matching word still gets
  intercepted, and the double-Enter ghost remains.
- **(3) Both + upstream PR** — RECOMMENDED. Do (1) now, file Patch A+B as a
  reedline PR, un-vendor when it lands (add a Watch section here like
  tiny_http's, with the `gh pr view` check).

Repro for verification, before and after: type `create or replace ta`, Tab
(menu opens), keep typing the rest of the statement, Enter — a keyword is
appended at end of line instead of the statement running.
