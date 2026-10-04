# Agent rules

DuckTable is a fast, minimal native Mac client for DuckDB, built in Rust on
GPUI. It speaks only to DuckDB Harbor: it never links DuckDB, never opens a
database file, and reaches every database through a Harbor server. It
targets Apple silicon only. It never writes to a database until the user says
so, never loses a staged change, and never lets the user believe something
landed that did not; every rule below serves that or the native feel.

`docs/DESIGN.md` is the product and the code layout, `docs/EDITING.md` the
grid's laws and its one dialog, `docs/QUERY.md` the Query view and its
transactions, and `docs/UPDATES.md` releasing and Sparkle. The repository's
`HANDOFF.md` covers both products, how the owner works, and what is open:
read it first.

## Shape

- `crates/ducktable`: the app. `app.rs` is the root entity and the only
  mutator of connection state; the surfaces (`sidebar.rs`, `content.rs`,
  `grid.rs`, `structure.rs`, `inspector.rs`, `query.rs`, `footer.rs`) read
  state and call back into it. `edits.rs` (the staging model) and `sql.rs`
  (every hand-written query) are pure. Colors resolve through `theme.rs`.
  `main.rs` holds the entry point, menus, key bindings and the quit dialog;
  `updater.rs` is the Sparkle glue.
- `crates/harbor-client`: HTTP to Harbor over unix sockets and TCP, sessions,
  and the fleet (which databases exist and which server serves each).
- `crates/duckdb-lang`: the tree-sitter grammar the Query editor highlights
  with.
- Harbor's `wire` and `common` crates are built from the sibling `harbor/`
  tree, so the protocol is checked on both sides of every commit.

A rule with a decision in it belongs in a pure function, with a unit test.

## Rules

- Nothing writes until ⌘S, and a commit is one all-or-nothing transaction.
  Every UPDATE and DELETE is keyed by the row's original key values (or its
  rowid and a hash of the whole row) and must affect exactly one row.
- A commit whose answer never came is held, not guessed: its staged set is
  judged whole ("It landed" or "It did not land"), never row by row.
- Anything a quit would lose is counted in `QuitRisks`, and the dialog says
  it. Any state that holds unsent work is counted there, with a test.
  ⌘Q, the menu's Quit, the close button and the updater's Install and
  Relaunch all ask; the Dock's Quit and a logout cannot until GPUI has a
  should-quit hook.
- While the quit dialog is up, nothing replaces what is behind it: actions
  that would switch databases, tables or views check `asking_to_quit`.
- The Query view reads statement keywords with `wire::statement`, never a
  parser of its own. Only an answer to `SELECT 1` on the session sets the
  transaction band.
- gpui-kit is never vendored. DuckTable pins the shreeve/gpui-kit fork by
  the commit of a `v0.7.0-patched.N` tag, the same rev for all five kit
  crates in `[patch.crates-io]`, or they do not resolve together. A change
  DuckTable needs from the kit is made in the fork, generic rather than
  DuckTable's, by the GPUI session that owns it; DuckTable does not patch
  around the kit. When a pin move regresses something, send that session a
  repro and stay on the older tag.
- `flex_none()` in gpui-pre also resets `flex_basis`; a fixed panel uses
  `flex_grow_0().flex_shrink_0()`. gpui-kit's `DialogClose` fills the width
  it is given, so it sits in a box of its own.
- Never run `cargo fmt` across the tree; format only the lines you wrote.
- Never point the app or a probe at the MedLabs database. Use a scratch
  file.
- `notes.txt` at the repository root is gitignored and holds the Sparkle
  private key's backup: never read it, never commit it.
- Releases go through `scripts/release.sh` and `scripts/update-cask.sh`
  only. Never re-sign, rename or copy over an installed copy: the bundle
  signs as `com.shreeve.ducktable`, the name macOS keys Local Network
  permission to.

## Check

```bash
cargo check --workspace --all-targets
cargo test --workspace
```

The live probes need a Harbor and a scratch database, and run only against
the file `HARBOR_LIVE_DB` names:

```bash
HARBOR_LIVE_DB=/tmp/scratch.duckdb cargo test -p harbor-client --test live -- --ignored
```

`an_open_database_outlives_harbors_linger` also needs `HARBOR_FIXTURE`. Give
the probes their own `HARBOR_HOME` under `$TMPDIR`: a unix socket path has
104 bytes, and a scratchpad path runs past it.

To try a change, run `target/release/ducktable <scratch.duckdb>`, or build
the signed bundle with `scripts/macos-app.sh release`, which leaves
`target/DuckTable.app`. Copy a bundle somewhere scratch before opening it,
never over `/Applications/DuckTable.app`.
