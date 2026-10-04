# Handoff — working in this repository

Read this first. It says what the repo is, how its owner works, how the
pieces fit, how to build, test and release, and what is open. Everything
here was true on 2026-10-04 at harbor v0.44.2 and DuckTable v0.22.9; the
changelog and git history are the record after that.

## What this is

Two products, one repository, each its own Cargo workspace with its own
version and release tags:

- **`harbor/`** — DuckDB Harbor. One small binary that serves a DuckDB file
  over plain HTTP to any client, and is its own REPL. Tags are `v*`.
- **`ducktable/`** — DuckTable. A native macOS client for Harbor servers,
  for Apple silicon, in Rust on GPUI through gpui-kit (pinned to Steve's
  fork; see "DuckTable and the gpui-kit fork"). Tags are `ducktable-v*`. It
  builds Harbor's protocol crates from the sibling tree, so the wire
  contract is checked on both sides of every commit. `ducktable/AGENTS.md`
  holds its rules, and `ducktable/docs/` its design, editing, query and
  update contracts.

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
- **Work in a worktree.** The checkout at `~/Data/Code/duckdb-harbor` is
  shared: another session may have a branch out and files uncommitted in
  it, and a `git switch` there moves its work under it. Look first
  (`git branch --show-current`, `git status --short`, `git reflog -4`),
  never `git add -A` there, and do the work in
  `~/Data/Code/duckdb-harbor-wt/<slug>` (`git worktree add`), removed when
  its branch lands. `harbor/` and `ducktable/` are often worked by two
  sessions at once: each lands what it touched, and says so to the other
  before touching a file both use.
- **Public text is Steve's.** An issue, a comment or a release note goes
  out under his name: his voice, plain, and posted on his word.
- **Scratch only.** Probes run on a scratch database with a short
  `HARBOR_HOME` (`/tmp/x`: a unix socket path has about a hundred bytes),
  stopped when done. Never the MedLabs database, here or on `live`.

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
  `http.rs`, `snapshot.rs` (the renewable session a backup runs on),
  `installs.rs` (finding a second copy of harbor on the machine), and
  `src/update.rs` (`harbor update`, which runs `install.sh` and reports
  servers on old code). The client is an HTTP client of its own server and
  never touches DuckDB.

Three more first-party crates: `common` (paths, config, membership,
autostart, permissions — shared with DuckTable), `wire` (protocol types,
protocol version 1, and `statement`: which keyword the engine will act on,
read once for the server, the CLI and DuckTable), `justhttp` (the
synchronous HTTP/1.1 server over TCP and unix sockets; a first-party fork).

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
- **A transaction lives in a session, and its end is told truly.** `BEGIN`
  without a `sessionId` is a `400`, as `USE` is: the pooled connection goes
  back when the request ends. The CLI opens a session at `BEGIN` and holds
  it to the `COMMIT` or `ROLLBACK` that ends the transaction; the REPL
  keeps it past the thirty-second idle limit with a `SELECT 1` every ten
  seconds, and the five-minute ceiling ends it. A `COMMIT` on a transaction
  an error or a cancel aborted is rolled back by the server and answered
  `400` saying so, where the engine alone answers success. A `COMMIT` runs
  to its answer: a cancel lands before it starts or not at all, so a `499`
  means nothing was kept. A streaming statement its reader cut short leaves
  its transaction aborted. All of it is measured in `regressions` and
  `cancel`.
- **The TCP door is for programs.** A request with an `Origin`, or a `Host`
  that is not `localhost` or an IP literal, is a `403`: a web page in the
  operator's browser must not reach a loopback listener. The unix socket
  takes no such check.
- **Two installers, one copy.** `install.sh` puts harbor in `~/.local`;
  Homebrew's `duckdb-harbor` formula keeps its own under `libexec`. Each
  upgrades only its own (`harbor update`, `brew upgrade duckdb-harbor`), so
  the formula refuses to install beside a script copy, `harbor update`
  refuses on a Homebrew copy, and `harbor` names a second copy when it
  finds one.
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
all of them on every push to main and nightly at 23:41 UTC. A pull request
gets three checks: the quick gate, `windows` (build, the listing's note,
and a query over the URL form on `windows-2025`), and `ducktable check`,
which compiles DuckTable against harbor's crates. A full local run is ten
to twelve minutes.

Two things bite locally:

- The `sessions` suite needs a `duckdb` CLI at least as new as the engine's
  file format. A laptop CLI older than the pinned engine fails "the
  committed row survived" for environmental reasons while CI is green.
- The full suite wants `sample.duckdb`; make it with
  `test/scripts/fixture.sh sample.duckdb`.
- `catalog` checks the server's `harborVersion` against `Cargo.toml`, so a
  version bump made while a run is under way fails it against the binary
  built before the bump.
- `harbor <db> start` serves in place when stdin or stdout is not a
  terminal. A script that wants a server puts it in the background and
  ends it with `harbor <db> stop`.

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
Every release since 0.39.0 carries alpha42289 on all five platforms, and
the engine release holds those same libraries byte for byte.

The fetch script and `Release.yml` decide "can this engine serve harbor" by
looking for `duckdb_v2_create_environment`, the symbol the loader gates on
in `engine/mod.rs`. The port to the reworked API changes what the loader
gates on, and those two greps change with it.

The way forward is to port to the reworked API once DuckDB's naming settles
(re-run `gen-v2-ffi.rb` against the new spec, fix the seventeen call sites),
then set the variable to `latest`. The API is still moving: duckdb/duckdb#26230
passes structs by pointer, which touches thirty-three of harbor's bindings,
and the draft duckdb/duckdb#25865 reworks query results, the calls behind
`statement_execute`, `result_step` and `result_fetch_chunk`. The port waits
for the result API to settle; DuckDB 2.0.0 is scheduled for 2026-10-21. A
clone of DuckDB for reading the source is at `~/Data/Code/duckdb`. Upstream
issues harbor has filed and watches: duckdb#25282 (nap race), duckdb#25301
(prepare cost), duckdb#25967 (deeply nested VARIANT: quadratic `UPDATE`,
segfault in the cast). duckdb-rs#841 is resolved. A DuckDB VARIANT cast bug
we hit is duckdb#25873.

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
6. Install and prove it: `harbor update`, `harbor --version`, one real
   query on a scratch database. `harbor update` ends by naming each server
   still on the code it started with.
7. Point the Homebrew formula at it: `scripts/update-formula.sh X.Y.Z` opens
   the pull request on `shreeve/homebrew-tap` (the checkout beside this
   repository, or `TAP=`); land it. `brew upgrade duckdb-harbor` then
   installs the version. The formula keeps the binary and its `libduckdb`
   under `libexec` and launches through `opt`, so a login item survives an
   upgrade; `harbor update` refuses on a Homebrew copy and names `brew
   upgrade`. Check the three checksums in the pull request against
   `harbor-vX.Y.Z-checksums.txt`; tap pull requests are squash-merged.

Patch versions are for fixes and refinements; a new capability is a minor
bump (brace expansion was 0.40.0). A documentation-only change needs no
version and no changelog entry.

DuckTable releases run locally, signed with the Developer ID and notarized,
the way Shotts and Transfer release (`ducktable/docs/UPDATES.md`, "Cutting a
release"). Land the changelog entry first, and the Sparkle pin when Sparkle
has a newer stable release — every DuckTable release ships the latest stable
Sparkle, never a beta: set `sparkle_version` and `sparkle_sha256` together
in `scripts/sparkle.sh`, the path in `docs/UPDATES.md`, and say so in the
changelog. The changelog heading carries the day the release ships. Release
harbor first when both ship, since the lockfile records harbor-common and
wire. Then, from a clean `main` in step with origin, in `ducktable/`:
`scripts/release.sh X.Y.Z --dry-run`, `scripts/release.sh X.Y.Z`, and
`scripts/update-cask.sh X.Y.Z`, and merge the tap's pull request
(`Casks/ducktable.rb` on `shreeve/homebrew-tap`) once its checksum matches
the release's `DuckTable-X.Y.Z.zip`. The release is the one job done in the
shared checkout, since the script refuses anything but `main`: look first,
as for any work there. The release script sets the version and runs `cargo update -w`,
commits "DuckTable X.Y.Z" straight to `main` with an annotated
`ducktable-vX.Y.Z` tag, publishes the versioned release (never `--latest`)
and rewrites the `ducktable-updates` feed; confirm `appcast.xml` lists the
new version first. That feed release stays a prerelease so
`/releases/latest` remains harbor's. The bundle signs as
`com.shreeve.ducktable`, the name macOS keys Local Network permission to;
never re-sign, rename or copy over an installed copy — both installers swap
the bundle in by rename (`docs/UPDATES.md`, "The bundle's identity").

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
Steve upgrades it himself with `ssh -t live 'harbor update --restart'`,
which installs the release and restarts the unit; `systemctl --user restart
harbor-medlabs` restarts it alone, and nothing kills it. The MedLabs app
rides through a harbor restart. The ssh session closes at "restarting
medlabs", before the last line prints, and the unit comes back about
fifteen seconds later, so look after that, not at once: `harbor` (the
listing, whose VERSION column is the running server's),
`systemctl --user is-active harbor-medlabs`, and
`cd ~/src/medlabs && rip sites status medlabs --json`.

## DuckTable and the gpui-kit fork

DuckTable draws its UI with gpui-kit (GPUI, gpui-pre 0.3.7, and Longbridge's
component library), and never vendors it. It builds against Steve's fork,
`shreeve/gpui-kit`: the branch `patched/0.7` is Longbridge's main with the
fork's fixes on top, each tagged `v0.7.0-patched.N`. The fork's checkout,
`~/Data/Code/gpui-kit`, belongs to the GPUI session, which owns the branch,
its tags and the upstream pull requests, and tracks them in
`.upstream/TRACKING.md` there. Other sessions read it and change nothing in
it.

- **One rev for five crates.** `ducktable/Cargo.toml` pins gpui-kit,
  gpui-base, gpui-component, gpui-component-macros and gpui-kit-assets in
  `[patch.crates-io]` to the commit of one tag. A crate from a git source
  takes its siblings from that source, so a mixed set does not resolve.
- **Moving the pin** is its own pull request: replace the rev in all five
  lines and the tag named in the comment, `cargo update -p` the five
  crates, confirm `Cargo.lock` names only that rev, then build, test and
  look by hand at the Structure view's DDL card, ⌘- and ⇧-click selection,
  Home and End, column widths, the Query editor's line numbers and send
  mark, the sidebar's width on a window resize, and the inspector's
  divider. A regression goes to the GPUI session as a repro against the tag
  that showed it, and DuckTable stays on the tag it had.
- **A change DuckTable needs from the kit is made in the fork**, generic
  rather than DuckTable's, by the GPUI session, and reaches DuckTable as a
  later tag. DuckTable does not patch around the kit.
- **GPUI has no should-quit hook.** Its app delegate registers
  `applicationWillTerminate:` and not `applicationShouldTerminate:`, so
  Quit from the Dock's menu and a logout end DuckTable without its quit
  dialog. Steve is raising it with Zed; the GPUI session tracks it as item
  11 of `TRACKING.md`, with an Objective-C runtime fallback if Zed does not
  add one. The updater's Install and Relaunch asks anyway, through
  Sparkle's own delegate.

## Reedline

The REPL's line editor is reedline from crates.io, 0.52 or later, used as
published. Four upstream behaviors hold the REPL's key rules up, and a
reedline upgrade is checked against them:

- A completion menu with no suggestions does not swallow Enter
  (nushell/reedline#1175).
- A menu closes at the end of the word it was opened for
  (nushell/reedline#1209). The boundary belongs to the menu, so harbor's
  completion menu is built `.with_word_chars("_.")` in `interactive.rs`: a
  `.` keeps it open for a qualified name, and anything else that cannot
  extend an identifier closes it.
- `ReedlineEvent::MenuAccept` takes a completion without submitting, and
  reports `Inapplicable` with no active menu, so it composes under
  `UntilFound` (nushell/reedline#1203). Tab is bound to it.
- `Up` and `Down` report `Inapplicable` when they moved nothing
  (nushell/reedline#1226). That is what lets Down on the live line fall
  through to opening the completion panel.

The proof after any upgrade is the repro: `create or replace ta`, Tab, keep
typing, Enter. The statement must run with no stray word appended; type the
tail fast enough to arrive as one batch. The pty driver in
`test/scripts/lifecycle.sh` (the mooring check) answers the two terminal
probes the REPL makes on startup and is the pattern for scripting it.

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
stays one record per line. `NaN` and `Infinity` stay strings; a document is
spliced whole however deep it nests, since the well-formedness check skips
the value without recursing. The wire is untouched by any of this.

## VARIANT, in one line

The canonical reference for reading and writing VARIANT from SQL, Rip and
the REPL is `rip/docs/VARIANTS.md` in the rip repository, measured against
the engine build live runs. Read it before probing. The rule that explains
the rest: objects in, values out; a bare string written without `::JSON` is
stored as a string and every path into it is NULL, silently. harbor closes
that for one case only: an object or array in `params` aimed at a VARIANT is
bound as the document (`Conn::bind`: one bind pass, skipped unless a param is
an object or array, then two casts through JSON and VARIANT types the
connection makes once; `run_statement` asks its slot for a cancel between
`bind` and `execute`, because the engine drops an interrupt that lands during
either, and a cancel found there leaves the transaction aborted as a later
one would: `Conn::abort_transaction`). A string param is never read as JSON — a client
holding JSON text casts it, which is what Rip's ORM and DuckTable do. Deeply
nested JSON is the one input that hurts the engine through a VARIANT
(duckdb#25967, measured on the alpha42779 CLI): an `UPDATE` of a VARIANT
column costs the square of the nesting depth, 0.6 s at 1,000 levels and 15 s
at 5,000, where an `INSERT` of the same value takes 10 ms; and the cast
segfaults at about 40,000 levels, the `UPDATE` at 20,000. On alpha42289 a
SQL-side `doc::JSON` over a stored 5,000-level document ends the harbor
process, while returning the VARIANT cell itself is fine. Every layer keeps
one number: an object or array param nests at most 100 levels, its own levels
counted, and harbor answers one more with a 400 before anything is bound
(`json_to_duckdb`), as Rip's ORM and DuckTable's editor refuse the same
document before it is sent. Only a string the statement casts through
`?::JSON`, which no layer inspects, reaches any of this.

## Recent history, for orientation

| version | date | change |
|---|---|---|
| 0.42.0 | 09-22 | the TCP door refuses a browser; keyless rows by hash; infinite dates as `infinity`; a brace-expansion budget |
| 0.42.1 | 09-23 | reedline in step with upstream |
| 0.43.0 | 09-23 | `harbor update [version] [--check] [--restart]` |
| 0.43.1 | 09-26 | reedline 0.52 from crates.io; the vendored copy is gone |
| 0.43.2 | 10-03 | on Windows the list says why a running server is not in it |
| 0.43.3 | 10-03 | `EXPLAIN` prints the plan as the engine drew it |
| 0.43.4 | 10-03 | a plan is colored the way DuckDB's shell colors it |
| 0.43.5 | 10-03 | Homebrew installs harbor; `harbor` names a second copy on the machine |
| 0.44.0 | 10-03 | a transaction holds from the CLI; `BEGIN` needs a session; the review's remaining defects |
| 0.44.1 | 10-04 | a `COMMIT` on an aborted transaction says it was rolled back; the REPL keeps its session alive |
| 0.44.2 | 10-04 | a `COMMIT` runs to its answer, so a `499` means nothing was kept |

Older milestones the code still reflects: 0.20 collapsed everything into
one binary with the refcounted lifetime; 0.21 moved to the direct v2 C API
and retired duckdb-rs; 0.33 gave autostart the brew-services model; 0.34
added `--block-size` and fixed backup/restore; 0.39 is where VARIANT became
usable end to end; 0.40 put brace expansion in the server; 0.41 bound an
object or array param aimed at a VARIANT as the document, to a hundred
levels.

## Open items

- **Port to DuckDB's reworked v2 C API**, then set the
  `DUCKDB_LIB_BUILD` repository variable to `latest`. Wait for the naming to
  settle; there was reviewer discussion upstream about instance-versus-
  database option scope.
- **A binary wire mode** is parked until DuckDB GA.
- **The deployment runbook** (`duckdb-harbor-runbook`) is deferred to GA;
  four decisions were recorded so they are not re-derived.
- **DuckTable** is at 0.22.9, early and moving fast; its own docs are under
  `ducktable/docs/`. Its Sparkle signing key lives in the login keychain
  under the account `ducktable`, with a backup in the gitignored, untracked
  `notes.txt` at the repo root. Never read or commit that file. Open:
  - *EXPLAIN output* (#132): the grid shows a plan's first line, which is a
    box border, and the inspector right-aligns the plan. It wants a
    preformatted, full-width view.
  - *A called-off Open Database URL* is dialed again on Cancel as a plain
    remote connect, without the config entry being saved again. It wants an
    `Aim` of its own.
  - *A COMMIT answered 499* is read as in doubt. A session's COMMIT runs to
    its answer (harbor 0.44.2), so a 499 there means nothing was kept, and
    the held set's verdict could say so.
  - *Paths reviewed and unit-tested but not yet watched on screen:* the held
    set's two verdicts after a COMMIT that got no answer, and Install and
    Relaunch held by the quit dialog against a real update.
- **Windows cannot list its servers.** The list finds servers by unix
  socket and Windows has none; the list says so and gives the URL form.
- **Linux glibc floor.** Release archives are built on Ubuntu 24.04 and
  need glibc 2.39; the README and the release notes say so. Building on an
  older baseline (a manylinux container or cargo-zigbuild) would run on
  older distributions without changing harbor, and is deliberately not
  scheduled; issue #56 was closed with an offer to revisit if it blocks
  someone.
- **A cleanup pass, when there is a session for one.** The code review of
  2026-09 closed its defects through 0.44.2; what it left is recorded here
  and in the issues, none of it urgent.
  - *A statement keeps running after its client disconnects before any rows
    are sent* (#133). The server learns a client is gone only at a failed
    write. The connection's reader already sees the close; the work is
    carrying that to the request being answered without reading a half-close
    or a pipelined request as a departure. A statement deadline bounds it
    meanwhile.
  - *The REPL's and DuckTable's keep-alive is a statement.* `SELECT 1` on
    the session every ten seconds counts in `/sessions` and overwrites a
    session's profiling output. A touch that runs nothing, a renew for an
    ordinary session that resets its idle clock and leaves its ceiling
    alone, would replace both.
  - *`harbor update --restart` over ssh ends without its last line.* The
    session closes once the restart begins and "restarted" is never
    printed, though the restart completes. Cause not looked into.
  - *Left by the review on purpose:* workflow actions pinned by tag, not
    commit; the engine layer's types that can outlive what they point to,
    which is why `cargo clippy` fails on harbor, and its slow VARIANT cell
    path, both of which the port to the reworked v2 API rewrites; type names
    built by hand beside the engine's; the `[defaults]` config section,
    parsed and read by nothing; and era framing in older comments in
    `lib.rs`, `wire` and `justhttp`.
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
  `~/.local/bin`, engine in `~/.local/lib`. Its `~/.zshenv` puts
  `~/.local/bin` and `~/.bun/bin` on PATH for a non-interactive
  `ssh live '…'`, which reads no other rc file. It is a Google Cloud host,
  not on the LAN.
