# Handoff — working in this repository

Read this first. It says what the repo is, how its owner works, how the
pieces fit, how to build, test and release, and what is open. Everything
here was true on 2026-09-18 at harbor v0.40.1; the changelog and git history
are the record after that.

## What this is

Two products, one repository, each its own Cargo workspace with its own
version and release tags:

- **`harbor/`** — DuckDB Harbor. One small binary that serves a DuckDB file
  over plain HTTP to any client, and is its own REPL. Tags are `v*`.
- **`ducktable/`** — DuckTable. A native macOS client for Harbor servers
  (Rust + GPUI). Tags are `ducktable-v*`. It builds Harbor's protocol crates
  from the sibling tree, so the wire contract is checked on both sides of
  every commit.

The root `README.md` is the front door to both. `harbor/README.md` is the
product's full manual and is kept current; `harbor/PRODUCT.md` explains the
design decisions; `harbor/SALES-PITCH.md` positions it. `install.sh` and
`install.ps1` at the root are the quick installers.

The owner is Steve Shreeve. The main consumer of Harbor is his Rip runtime
(`~/Data/Code/rip`, the `rip/db` package and the ORM in
`src/runtime/orm.js` + `duckdb.js`) and the MedLabs application on top of
it (`~/Data/Code/medlabs`), which runs in production on the host `live`.

## How the owner works

These are standing rules, not preferences to weigh.

- **Timeless code and prose.** No "legacy", "backward compatibility",
  "we used to", "new in", or era framing anywhere in code, comments, docs
  or commit bodies. Delete the old thing; do not narrate the transition.
  Treat such phrasing as a defect in review.
- **No AI attribution.** No `Co-Authored-By`, no "Generated with" trailers,
  in commits, PR bodies or comments. This overrides any tooling notice.
- **Commit and push only when asked.** "Make 0.40.2", "release it", "land
  it" are asks. A question is a question; answer it and stop.
- **"Land" means** a true merge (`gh pr merge N --merge --delete-branch`),
  then delete the local branch too. Only `main` remains, ever. Check with
  `gh api repos/shreeve/duckdb-harbor/branches --jq '.[].name'`.
- **Verify empirically.** Claims about engine behavior are measured against
  the engine, not recalled. When a probe contradicts a belief, the probe
  wins and the doc gets corrected. Subagent reviews are welcome and have
  caught real bugs.
- **Commit messages** are prose: a one-line subject in the form
  `area: what it now does`, then paragraphs that say why, in complete
  sentences. Same voice as the changelog.
- **`live` is production.** Reads are fine. Writes, restarts and installs
  there happen on Steve's word, in the same conversation, not on a standing
  assumption.
- **Never run `cargo fmt` across the tree.** There is no rustfmt config and
  the code is deliberately not rustfmt-clean; a blanket format rewrites
  eighteen files. Format nothing but the lines you wrote, by hand.

## The shape of harbor

One crate, `crates/harbor`, holds both halves of the binary:

- **Server half** — `src/lib.rs` (HTTP routes, sessions, request guards),
  `src/engine/` (the DuckDB v2 C API: `ffi.rs` is generated, `conn.rs` is
  the connection, `encode.rs` turns vectors into NDJSON), `src/encode.rs`
  (the JSON-safe rules), `src/verbs.rs` (start/stop/attach/backup…),
  `src/backup.rs`, and `src/unbrace.rs` (brace expansion, applied to every
  statement before the engine sees it).
- **Client half** — `src/repl/`: `mod.rs` (the client, transports, dot
  commands), `interactive.rs` (reedline setup, keybindings, the statement
  splitter and Enter validator), `render.rs` (every output mode),
  `scan.rs` (the one lexer for strings/comments/dollar quotes, shared by
  the splitter, highlighter and unbrace), `complete.rs`, `highlight.rs`,
  `http.rs`. The client is an HTTP client of its own server and never
  touches DuckDB.

Three more first-party crates: `common` (paths, config, membership,
autostart, permissions — shared with DuckTable), `wire` (protocol types,
protocol version 1), `justhttp` (the synchronous HTTP/1.1 server over TCP
and unix sockets; a first-party fork).

Facts that shape everything:

- **Nothing links `libduckdb`.** The engine is loaded at runtime:
  `HARBOR_LIBDUCKDB` first, then `../lib` beside the binary and the binary's
  own directory, `~/.local/lib`, `~/.duckdb/cli/latest`, then the newest
  `~/.duckdb/cli/<version>`. The client half runs with no engine at all.
- **One process per database file.** DuckDB's own file lock is the mutex.
  There is no registry; the listening unix socket is the registration.
  `harbor <db>` on a terminal is the REPL and spawns a server behind the
  file if none is up (refcounted, leaves with its last client);
  `harbor <db> start` is an owned server that runs until `stop`.
- **Paths.** `~/.config/harbor/config.toml` is desired state (what is
  attached, per-connection options); `~/.local/state/harbor/runtime/` holds
  each server's `.sock` and `.log`, named `<basename>-<fnv1a32(path)>`.
  `harbor` alone lists what is running and attached.
- **Wire.** `POST /sql` takes one statement and streams NDJSON: a schema
  line per column, then rows. A `VARIANT` cell crosses as JSON text with
  `"encoding":"json"`; a `JSON` column by its type. Bodies cap at 8 MiB.
  Sessions (`/sql/sessions`) hold a connection for a transaction. See the
  README's endpoint table.
- **Every statement is brace-expanded in the server** (`unbrace.rs`):
  `r.{a, b}` becomes `r.a, r.b` for every client. A struct literal (a lone
  `:` at its top level), strings, quoted identifiers, dollar quotes,
  comments and unbalanced braces are left alone.

## Build, test, run

```bash
cd harbor
make harbor            # cargo build -p harbor --release → target/release/harbor
make unit              # cargo test --release --workspace --all-features
make test              # test/scripts/check.sh: unit plus the thirteen integration suites
make test SUITES="regressions spec"      # a subset
make fetch-duckdb      # engine + CLI + headers into ~/.duckdb/cli/2.0.0
make install           # copy the binary to ~/.local/bin
```

The suites live in `test/scripts/` and each file's header says what it
proves. The ones to reach for: `regressions` (request isolation, settings,
limits), `spec` (the wire encoding, spelled out), `types` (every DuckDB
type), `hostile` (adversarial HTTP input, statement smuggling), `roundtrip`
(backup/restore fidelity), `asserts` (answers checked against an independent
oracle, curl in and NDJSON out), `sessions`, `cancel`, `catalog`, `lifecycle`,
`stress`, `fuzz`, `deployment`. CI's quick gate runs
`unit lifecycle types spec catalog sessions cancel`; `FullSuite.yml` runs
all of them on every push to main and nightly at 23:41 UTC.

Two things bite locally:

- The `sessions` suite needs a `duckdb` CLI at least as new as the engine's
  file format. A laptop CLI older than the pinned engine fails "the
  committed row survived" for environmental reasons while CI is green.
- The full suite wants `sample.duckdb`; make it with
  `test/scripts/fixture.sh sample.duckdb`.

To drive the REPL non-interactively for a proof, use `expect` and answer the
two terminal probes it makes on startup (cursor position `ESC[6n` → reply
`ESC[1;1R`; background color `OSC 11;?` → reply `OSC 11;rgb:0000/0000/0000`).
A working script from the last session is the pattern.

For a scratch database, use a file in a scratch directory
(`harbor ./x.duckdb --mode csv -c "…"` or `< file.sql`), and `harbor
./x.duckdb stop` when done. Never point probes at the MedLabs database.

## The engine

Harbor binds DuckDB's **v2 C API**; DuckDB 2.0 is the floor. `ffi.rs` is
generated from DuckDB's `api_spec/v2` by `scripts/gen-v2-ffi.rb`, never
transcribed from the header. The engine comes from DuckDB's nightly channel
of the 2.0 branch (`artifacts.duckdb.org/v2.0-cyanoptera`) via
`scripts/fetch-duckdb.sh`.

**The engine is pinned right now.** On 2026-09-17 the nightly reworked the v2
API (duckdb/duckdb#25751: `open` → `database_create` + `database_attach`,
`connect` → `connection_create`, options by name, seventeen renames) and
harbor's loader refuses it ("engine has no v2 C API"). The repository
variable `DUCKDB_LIB_BUILD=alpha42289` makes the fetch script take
`libduckdb` from the `engine-alpha42289` release of this repository (one
`libduckdb-<plat>.tar.gz` per platform, a pre-release so `/releases/latest`
never points at it) while the CLI and headers still come from the channel.
`Tests.yml`, `FullSuite.yml` and `Release.yml` all honor it, and so does a
local `DUCKDB_LIB_BUILD=alpha42289 make fetch-duckdb`; without it a local
fetch refuses the channel's engine. `latest`, or no value, is the channel.
Every release from 0.39.0 through 0.40.1 carries alpha42289 on all five
platforms, and the engine release holds those same libraries byte for byte.

The fetch script and `Release.yml` decide "can this engine serve harbor" by
looking for `duckdb_v2_create_environment`, the symbol the loader gates on
in `engine/mod.rs`. The port to the reworked API changes what the loader
gates on, and those two greps change with it.

The way forward is to port to the reworked API once DuckDB's naming settles
(re-run `gen-v2-ffi.rb` against the new spec, fix the seventeen call sites),
then set the variable to `latest`. DuckDB 2.0 GA is expected in the second half of
October 2026. Upstream issues harbor has filed and watches: duckdb#25282
(nap race), duckdb#25301 (prepare cost), duckdb-rs#841. A DuckDB VARIANT
cast bug we hit is duckdb#25873.

## Releasing

Every feature is a PR from a branch off main; the version bump is a second,
separate PR. The flow, which the last three releases followed exactly:

1. Branch `feat/<slug>`, commit, push, `gh pr create`. The changelog entry
   goes in this PR, under a heading `## X.Y.Z — YYYY-MM-DD` at the top of
   `harbor/CHANGELOG.md`, written as prose bullets that say what changed and
   why.
2. Wait for both CI runs (`Tests`, `DuckTable`) on the head commit. A
   force-push cancels the old runs; watch the new ones by head SHA, not by
   the ids you first saw.
3. Land it.
4. Branch `release/harbor-X.Y.Z`: set `version` in `harbor/Cargo.toml`, run
   `cargo update -w` so `Cargo.lock` follows, commit "Release Harbor X.Y.Z",
   PR, CI, land.
5. `git tag -a vX.Y.Z -m "Harbor X.Y.Z"` on the merge commit, push the tag.
   `Release.yml` builds five archives (linux amd64/arm64, osx arm64, windows
   amd64/arm64) plus checksums and smoke-tests each; `/releases/latest`
   then points at it.
6. Install and prove it: the one-liner, `harbor --version`, one real query.

Patch versions are for fixes and refinements; a new capability is a minor
bump (brace expansion was 0.40.0). A documentation-only change needs no
version and no changelog entry.

DuckTable releases are one commit on main titled "DuckTable X.Y.Z": bump
`version` in `ducktable/Cargo.toml`, run `cargo update -w` there (the
lockfile also records harbor-common and wire, so release harbor first when
both ship), add the changelog entry, and bump the Sparkle pin — every
DuckTable release ships the latest stable Sparkle, never a beta: set
`sparkle_version` and `sparkle_sha256` together in `scripts/sparkle.sh`,
the path in `docs/UPDATES.md`, and say so in the changelog. Prove the bundle
with `scripts/macos-app.sh release` (check the embedded
`Sparkle.framework` version), then push an annotated `ducktable-vX.Y.Z` tag.
The script fails unless the app and its executable sign as
`com.shreeve.ducktable`, the name macOS keys Local Network permission to;
never re-sign, rename or copy over an installed copy — both installers swap
the bundle in by rename (`docs/UPDATES.md`, "The bundle's identity").
`DuckTableRelease.yml` publishes `DuckTable.zip` on the versioned release and
rewrites the `ducktable-updates` feed; confirm `appcast.xml` lists the new
version first. That feed release stays a prerelease so `/releases/latest`
remains harbor's.

The install one-liner, everywhere:

```bash
curl -fsSL https://raw.githubusercontent.com/shreeve/duckdb-harbor/main/install.sh | bash
```

It puts `harbor` in `~/.local/bin` and `libduckdb` in `~/.local/lib`. A
running server keeps its old binary until restarted; a server-side change
(anything in `lib.rs`, `engine/`, `unbrace.rs`) needs the restart, a
client-side change (`repl/`) does not.

On `live`, harbor runs under a user systemd unit, `harbor-medlabs.service`,
serving `~/src/medlabs/api/db/medlabs.duckdb` on `http://127.0.0.1:9495`.
Restart it with `systemctl --user restart harbor-medlabs`, never by killing
it. The MedLabs app rides through a harbor restart. Check afterwards:
`harbor` (the listing), `systemctl --user is-active harbor-medlabs`, and
`cd ~/src/medlabs && rip sites status medlabs --json`.

## The vendored reedline

`harbor/vendor/reedline` is reedline 0.50.0 wired through
`[patch.crates-io]`, carrying four patches in `src/engine.rs`, each with
tests and each described in `vendor/reedline/HARBOR.md` with its upstream
status. Patch D (Down reports itself inapplicable on the live line, so the
completion panel can open there and nowhere else) is not filed upstream.
The file is CRLF throughout; edit it with CRLF preserved and do not
reformat it. Its own suite: `cd vendor/reedline && cargo test --lib --
--test-threads=1` (1498 tests; the clipboard tests flake in parallel on
macOS, upstream's problem).

## REPL key rules, as settled

Up and Down walk history wherever history is, on a recalled line edited or
not, until it is emptied. On the live line at the bottom, Down opens the
completion panel, since there is nothing below to move to; open, Down moves
through it and Tab accepts. Ctrl-Space opens the panel anywhere. Tab
accepts a completion or the inline history hint; Right at end of line does
too. A statement submits on Enter only when it ends in `;`.

## Output modes, as settled

Display modes (duckbox, duckboxy, markdown, line, list) show a VARIANT
string bare; a JSON column shows its text with quotes, as DuckDB's own table
does. csv carries the JSON text CSV-escaped, so `42` and `"42"` stay apart
for a program. json and jsonlines splice a VARIANT or JSON cell in as JSON,
with newlines in a pretty-printed JSON column folded to spaces so jsonlines
stays one record per line. `NaN`, `Infinity` and documents deeper than 128
levels stay strings. The wire is untouched by any of this.

## VARIANT, in one line

The canonical reference for reading and writing VARIANT from SQL, Rip and
the REPL is `rip/docs/VARIANTS.md` in the rip repository, measured against
the engine build live runs. Read it before probing. The rule that explains
the rest: objects in, values out; a bare string written without `::JSON` is
stored as a string and every path into it is NULL, silently. harbor closes
that for one case only: an object or array in `params` aimed at a VARIANT is
bound as the document (`Conn::aim_documents`, one bind pass, skipped unless a
param is an object or array). A string param is never read as JSON — a client
holding JSON text casts it, which is what Rip's ORM and DuckTable do. JSON
text nested tens of thousands deep and cast to VARIANT recurses inside the
engine: 20,000 levels ran past a minute and about 70,000 killed the process
on the 16 MiB executor stack (measured on alpha42289). harbor's own request
parser stops at 128 levels, so only a SQL-side cast of a string reaches it.

## Recent history, for orientation

| version | date | change |
|---|---|---|
| 0.39.0 | 09-17 | VARIANT crosses the wire as JSON text, `encoding: json` |
| 0.39.1 | 09-18 | VARIANT strings unquoted in tables; engine pin `DUCKDB_ENGINE_RELEASE` |
| 0.39.2 | 09-18 | json/jsonlines emit VARIANT and JSON cells as JSON |
| 0.40.0 | 09-18 | brace expansion in the server |
| 0.40.1 | 09-18 | Down is history until there is no history below; Ctrl-Space lists |
| 0.40.2 | 09-20 | engine named by DuckDB build, `DUCKDB_LIB_BUILD`; fetch checks before it installs |
| 0.41.0 | 09-20 | an object or array param aimed at a VARIANT is bound as the document |

Older milestones the code still reflects: 0.20 collapsed everything into
one binary with the refcounted lifetime; 0.21 moved to the direct v2 C API
and retired duckdb-rs; 0.33 gave autostart the brew-services model; 0.34
added `--block-size` and fixed backup/restore; 0.39 is where VARIANT became
usable end to end.

## Open items

- **Port to DuckDB's reworked v2 C API**, then set the
  `DUCKDB_LIB_BUILD` repository variable to `latest`. Wait for the naming to
  settle; there was reviewer discussion upstream about instance-versus-
  database option scope.
- **Un-vendor reedline** when 0.52 ships with patches A–C, re-applying D on
  top (or filing it). `HARBOR.md` has the exact checklist.
- **A binary wire mode** is parked until DuckDB GA.
- **The deployment runbook** (`duckdb-harbor-runbook`) is deferred to GA;
  four decisions were recorded so they are not re-derived.
- **DuckTable** is at 0.22.1, early and moving fast; its own docs are under
  `ducktable/docs/`. Its Sparkle signing keys live in the gitignored,
  untracked `notes.txt` at the repo root and in the `SPARKLE_PRIVATE_KEY`
  repository secret. Never commit that file.
- **Linux glibc floor.** Release archives are built on Ubuntu 24.04 and
  need glibc 2.39; the README and the release notes say so. Building on an
  older baseline (a manylinux container or cargo-zigbuild) would run on
  older distributions without changing harbor, and is deliberately not
  scheduled; issue #56 was closed with an offer to revisit if it blocks
  someone.
- **Optional polish:** a distinct color for braces in the highlighter; the
  brace expander could take aliases inside a group if a syntax that does
  not collide with the struct-literal colon is chosen.

## Where the other repos fit

- **rip** (`~/Data/Code/rip`): the ORM's `variant` field type binds writes
  through `?::JSON` and the driver decodes any `encoding: json` or JSON
  column to JS values. `docs/VARIANTS.md` and `docs/ORM.md` there are the
  user-facing contracts for what harbor emits.
- **medlabs** (`~/Data/Code/medlabs`): the production consumer. Its
  `result_jsons.json`, `orders.raw_request`/`raw_response` and `events.data`
  are VARIANT columns as of 09-17. Patient data never enters that repo.
- **live**: `ssh live`. `~/src/medlabs`, `~/src/rip`, harbor in
  `~/.local/bin`, engine in `~/.local/lib`. Put `$HOME/.bun/bin` and
  `$HOME/.local/bin` on PATH in a non-interactive ssh command.
