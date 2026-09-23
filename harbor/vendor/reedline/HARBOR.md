# Harbor's vendored reedline

This is reedline's `main` at commit
`db34d84222a94686be30c638b4c629f58a97a6be` (2026-09-23, version 0.51.0 in
its `Cargo.toml`), taken with `git archive` and wired via
`[patch.crates-io]` in the workspace `Cargo.toml`. It carries no patches of
harbor's own: everything harbor needed is upstream. The copy exists only
because crates.io still serves 0.51.0 (2026-08-22), which predates all of it.

What harbor relies on, and where each landed:

- **A completion menu with no suggestions does not swallow Enter**
  (https://github.com/nushell/reedline/pull/1175). Enter falls through to
  submit instead of inserting a highlighted word at the end of the line.
- **A menu closes at the end of the word it was opened for**
  (https://github.com/nushell/reedline/pull/1209). Upstream scopes the
  boundary to the menu: harbor's completion menu is built
  `.with_word_chars("_.")` in `crates/harbor/src/repl/interactive.rs`, so a
  `.` keeps it open for a qualified name and anything else that cannot extend
  an identifier closes it. Every `InsertChar` in a batch is examined, so a
  burst of typing that arrives as one event still closes it. Upstream also
  counts `InsertNewline` as ending the word.
- **`ReedlineEvent::MenuAccept` takes a completion without submitting**
  (https://github.com/nushell/reedline/pull/1203), and reports
  `Inapplicable` with no active menu so it composes under `UntilFound`.
- **`Up` and `Down` report whether they moved anything**
  (https://github.com/nushell/reedline/pull/1226). `Down` on the live line
  is `Inapplicable`, which is what lets harbor's binding fall through to
  opening the completion panel. Upstream's rule is the plain one: a `Down`
  that changes neither the buffer nor the cursor is inapplicable. So `Down`
  on a recalled line that has been edited opens the panel rather than walking
  history back to the live line, since reedline's history walk ends at the
  first edit. A recalled line left as it came still walks.

Also in this snapshot and relevant to harbor: `Vi::new` takes the visual-mode
keybindings as a third argument
(https://github.com/nushell/reedline/pull/1214), and the `helix` feature
is no longer a default.

Tests that prove the four behaviors live in this copy's `src/engine.rs`
(`enter_with_an_empty_menu_submits_the_line`, `a_word_boundary_closes_the_menu`,
`menu_accept_only_accepts_an_active_selection`, and the `down_*` and `up_*`
history-walk tests). The suite runs in place: `cd vendor/reedline && cargo
test -- --test-threads=1`. Multi-threaded `--all-features` runs on macOS
segfault intermittently in the system-clipboard tests (parallel pasteboard
access); they do so on clean upstream too.

Leave the sources exactly as `git archive` produced them. A local change
here is a patch harbor has to carry, and the point of this copy is that it
carries none.

## Un-vendor when 0.52 ships

When crates.io serves a reedline release at or past this commit: delete
`reedline` from `[patch.crates-io]` and the `exclude` list in
`harbor/Cargo.toml`, `rm -rf vendor/reedline`, set the reedline version in
`crates/harbor/Cargo.toml` to that release, then `cargo test -p harbor &&
make test SUITES="unit lifecycle types spec catalog sessions cancel"` and
re-run the repro: `create or replace ta`, Tab, keep typing, Enter. The
statement must run with no stray word appended; type the tail fast enough
to arrive as one batch, or the boundary scan goes untested. A pty driver for
exactly this sits in `test/scripts/lifecycle.sh` (the mooring check), and
answers the two terminal probes the REPL makes on startup.

## Refreshing this snapshot before then

From a checkout of nushell/reedline with `upstream` pointing at it:

```bash
cd harbor && cp vendor/reedline/HARBOR.md /tmp/HARBOR.md && rm -rf vendor/reedline
mkdir vendor/reedline && git -C ~/Data/Code/reedline archive upstream/main | tar -x -C vendor/reedline
rm -rf vendor/reedline/.github vendor/reedline/.gitignore vendor/reedline/.typos.toml
cp /tmp/HARBOR.md vendor/reedline/HARBOR.md
```

Then update the commit hash above, and the reedline version in
`crates/harbor/Cargo.toml` if the snapshot's `Cargo.toml` moved past it: the
`[patch]` only applies when the copy's version satisfies that requirement.
