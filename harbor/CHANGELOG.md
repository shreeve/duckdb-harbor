# Harbor changelog

Harbor release tags use `vX.Y.Z`. Entries are ordered by release date,
newest first. Separately tagged DuckDB engine mirrors are build artifacts, not
Harbor releases, and are not included here.

## 0.41.2 — 2026-09-21

- **A document parameter nests at most 100 levels.** Rip's ORM and
  DuckTable's editor each refuse a document nested past 100 levels, and
  harbor stopped only where its request parser happened to: 125 levels bound
  and 126 was a 400 that said `recursion limit exceeded at line 1 column 178`.
  Every layer now keeps the one number. An object or array parameter is
  measured by its own levels (`{}` is one, `[[1]]` is two) where parameters
  are read, before anything is bound: 100 levels bind, store and read back,
  and 101 is a 400 `bad_request` saying `a document param nests at most 100
  levels`, with nothing stored. The parser's own refusal past 125 levels says
  the same words. A string parameter is never inspected: 150 levels of
  brackets through `?::JSON` bind and store as before. The unit tests and the
  asserts suite hold the boundary for arrays and objects, a deep branch that
  is not the first, and the string.
- **A cancel always aborts the transaction it lands in.** A cancel that landed
  while a statement executed left its transaction aborted, the engine's doing;
  one that landed earlier, while an object or array parameter was being bound
  or before an executor had picked the statement up, answered the same 499
  and left the transaction open, so a client that carried on and committed
  would commit a transaction with a statement missing. A 499 inside a
  transaction now means one thing. On both early paths harbor retires the
  statement's slot and then runs `SELECT error('cancelled')` on the
  connection, dropping the error: measured on the engine, inside a
  transaction that leaves `Current transaction is aborted (please ROLLBACK)`
  for every statement until the ROLLBACK, and a `COMMIT` sent instead rolls
  back; in autocommit it leaves nothing, and the next statement runs and
  commits; an interrupt still pending from the cancel does not reach it,
  since the engine clears that as a statement starts, and it fails with its
  own error; it costs about 80 µs, on the cancel path only. The connection
  needs no more resetting than it did, since a lease is replaced at release
  and a worker holds no transaction between requests. The cancel suite binds
  a 7 MB document inside a transaction, cancels during the cast, and finds
  the next statement refused, the earlier write gone after ROLLBACK, a
  careless `COMMIT` committing nothing, and a session in autocommit
  untouched; a unit test drives both early paths on a connection.
- **`harbor restore` loads a `VARIANT` column as documents, so a `CHECK` on
  one restores.** A table such as `CREATE TABLE g (id INT, v VARIANT CHECK (v
  IS NULL OR variant_typeof(v) LIKE 'OBJECT%'))` backed up without a word and
  could not be restored: `load.sql` landed each cell as a VARIANT string,
  `after.sql` decoded it afterwards, and the constraint was tested in
  between. The backup is unchanged, byte for byte: text files, `COPY` lines in
  `load.sql`, the decode in `after.sql`, so a stock `duckdb` imports the
  directory as before. What changed is the restore. `IMPORT DATABASE` is
  `schema.sql` and then each `COPY` of `load.sql` with its file joined to the
  directory, and nothing more, so harbor runs those statements itself and
  loads a table whose decode is in `after.sql` another way: the table's own
  `COPY`, options untouched, fills a staging table that differs only in
  holding the `VARIANT` columns as `VARCHAR`, one `INSERT … SELECT * REPLACE
  (v::JSON::VARIANT AS v)` moves the rows across, and the staging table is
  dropped. Every other column is parsed exactly as it was, and the table
  never holds a VARIANT string. Measured with a table-level and a column-level
  CHECK, a `NOT NULL` VARIANT, a quoted schema and table name, a child table
  with a foreign key, and DECIMAL, TIMESTAMPTZ, BLOB, LIST, STRUCT, MAP, ENUM,
  HUGEINT, UUID, INTERVAL and DOUBLE columns beside the VARIANT: every row
  `IS NOT DISTINCT FROM` its source with `variant_typeof` equal, the SQL NULL
  row included. That row travels as the JSON text `null` (duckdb#25873), and
  a file edited to hold the null marker there instead restores to the same
  NULL. It is also faster: a 300,000-row table of small documents restored in
  1.0 s where the COPY and the UPDATE took 9.2 s, to a file of the same size.
  A table written as parquet, and a directory with no `after.sql`, load by
  their `COPY` as before. A stock `duckdb` still cannot restore a table whose
  CHECK refuses strings from a text backup; the README says so, and the
  roundtrip suite holds both halves. Running the files itself, restore sends
  each statement of `schema.sql` as a request of its own, so each one meets
  harbor's one-statement rule and its 8 MiB request cap, where
  `IMPORT DATABASE` took the file whole.
- **Known, and not changed here:** a table that holds both a `VARIANT` and a
  `GENERATED` column backs up and does not restore, in this release as in
  0.41.1. The VARIANT export is a `SELECT *`, which writes the generated
  column into the data file, and no `COPY` takes it back: 0.41.1 fails at the
  CSV sniff, 0.41.2 with `table "o" has 4 columns but 5 values were supplied`.
  A table with a generated column and no VARIANT exports without it and
  restores on both.

## 0.41.1 — 2026-09-21

- **A document parameter costs its casts, not the making of two types.** An
  object or array parameter aimed at a VARIANT is cast through JSON to
  VARIANT, and 0.41.0 made both logical types from their names for every
  such parameter, about 1.2 ms apiece, where the casts themselves take
  microseconds. The connection now makes the two types once, on the first
  document it binds, keeps them beside its VARIANT-to-JSON caster and
  releases them with it, across a reset included. Over 500 keep-alive
  requests, `UPDATE t SET doc = ? WHERE id = 1` with an object parameter went
  from a median of 2.73 ms to 0.44 ms, against 0.37 ms for the same statement
  with a string; a 100-row `INSERT` of object parameters went from 232 ms to
  1.5 ms, against 1.4 ms with strings. A type the engine refuses to make, as
  inside an aborted transaction, is not kept: the text is bound, the
  statement reports the abort, and the next request asks again.
- **A cancel that lands while parameters are being bound is kept.** The
  engine drops an interrupt that arrives before a statement starts
  executing, and 0.41.0 asked its slot about a cancel after the bind pass but
  before the casts, so `DELETE /sql/queries/<id>` could answer
  `{"cancelled":true}` and the statement then ran to its deadline: with 300
  object parameters that stretch was 0.9 s, and a cancel 0.4 s in left a
  7-second statement running its full 7 seconds. Binding is now its own step
  (`Conn::bind`), and the slot is asked once the values are built and before
  anything runs. The same request answers 499 at 0.4 s; with one 7 MB
  document, whose cast alone takes a third of a second, a cancel at any
  moment of it answers 499 when the cast returns, and inside a transaction
  leaves the transaction open with its writes, since nothing ran. The cancel
  suite holds the 7 MB case, outside a transaction and inside one.
- **The README no longer recommends a `CHECK` on `variant_typeof`.**
  `CHECK (v IS NULL OR variant_typeof(v) LIKE 'OBJECT%')` refuses arrays,
  scalars and JSON strings along with the unparsed string it was offered
  against, and it breaks the default backup: the tsv backup succeeds and the
  restore fails with `CHECK constraint failed`, because `load.sql` lands each
  VARIANT cell as a string and `after.sql` decodes it afterwards.
  `--format parquet` backs up and restores such a table.
- **A `VARIANT` nested in a `LIST`, `STRUCT` or `MAP` is not backed up by
  either format**, and the README now says so where it said parquet carried
  it: the backup is refused whole, naming the table, and nothing is written.
- **A document parameter nests at most 125 levels.** The request parser reads
  127 and the body's `{ "params": [ … ] }` uses two; the README, this file and
  the suite said 128. Level 125 binds and stores, 126 is a 400, and the suite
  measures both. A string through `?::JSON` has no such limit. Rip's ORM and
  DuckTable's editor refuse a document past 100 levels, so nothing a
  first-party client writes reaches it.
- Also corrected in the README: a cancel that lands before a statement begins
  leaves its transaction as it was; through an object parameter `-0` stores
  as the DOUBLE `-0.0` and `1e400` is a 400 at the body parse, where the
  `?::JSON` text path stores `0` and `Infinity`; for a VARIANT inside a list,
  struct or map it is the nested schema entry that reads `"lossless": false`,
  not the column's; and the REPL's json modes splice a document of any depth,
  because the well-formedness check does not recurse.

## 0.41.0 — 2026-09-20

- **An object or array parameter aimed at a VARIANT is the document.**
  `"params": [{"a":1}]` went to the engine as its JSON text, a VARCHAR, and
  text cast to VARIANT is a VARIANT *string*: `SET doc = ?` stored a value
  that read back as JSON while every path into it was NULL, and
  `WHERE doc = ?` never found a document. harbor now asks the engine what
  each parameter expects — one bind pass, made only when a request carries
  an object or an array — and binds such a parameter as the document where
  the answer is VARIANT. Aimed anywhere else it is its JSON text as before:
  an untyped slot, a `VARCHAR` column, a `$1` used against two types, a
  `VARIANT[]`. A string parameter is a string wherever it goes, whatever it
  spells; a client holding JSON text still writes `?::JSON`, and statements
  that already cast are untouched, since the parameter then expects JSON.
  The bind pass drops an interrupt that lands during it, so the slot is asked
  again before the statement runs; a cast the engine refuses, as inside an
  aborted transaction, leaves the text and the statement reports the error.
  Reading a string as JSON was considered and measured out: JSON text nested
  tens of thousands deep, cast to VARIANT, recurses inside the engine until
  the process dies, and no string parameter should be able to reach that.
  An object or array cannot: the request parser reads 127 levels, the body's
  own `{ "params": [ … ] }` is two of them, and a document parameter nested
  past 125 is a 400.

## 0.40.2 — 2026-09-20

- **The engine is named by its DuckDB build.** `DUCKDB_LIB_BUILD=alpha42289`
  tells `scripts/fetch-duckdb.sh`, CI and `Release.yml` to take `libduckdb`
  build `v2.0.0-alpha42289` from the `engine-alpha42289` release of this
  repository, which holds that one library for each of the five platforms;
  `latest`, the default, is the channel's current build. It takes the place
  of `DUCKDB_ENGINE_RELEASE`, which named a harbor release to lift the
  library out of: what has to stay fixed is a DuckDB build, and the name now
  says which one. The script checks that the library it fetched says it is
  that build.
- **A fetch checks the engine before it installs it.** The script looked for
  the v2 C API after the library was already in place, and looked for
  `duckdb_v2_connect`, which is also the start of `duckdb_v2_connection_create`
  in the reworked API — so the channel's current engine passed, and harbor
  then refused it at dlopen. The check now runs on the downloaded library,
  asks for `duckdb_v2_create_environment`, the symbol harbor's loader gates
  on, and a library that fails it never lands. `Release.yml` asks for the
  same symbol.

## 0.40.1 — 2026-09-18

- **Down is history until there is no history below.** Up and Down walk
  history wherever history is: on a recalled line, edited or not, until it
  is emptied. On the live line at the bottom — what was typed before any
  Up, or a fresh line — Down has nothing to move to, and there it opens the
  completion panel; open, Down moves through it and Tab accepts. Ctrl-Space
  opens the panel anywhere. Before, Down opened the panel whenever it could,
  which took the key away from history navigation. Reedline reports Down as
  inapplicable on the live line (vendored Patch D) so the binding can fall
  through to the panel.

## 0.40.0 — 2026-09-18

- **Brace expansion.** A statement can carry the shell's `{a,b}`, expanded
  in the server before the engine sees it: `raw_request.{requisitionNumber,
  visitDate, patient.{lastName, firstName}}` is four path expressions,
  joined with `, `. Items are separated by commas, or by whitespace alone;
  a group nests; it can sit anywhere in a term (`orders_{2025,2026}`,
  `r.{a,b}::VARCHAR`) and two groups in one term multiply. A struct literal
  — a group with a lone `:` at its top level — is left as it came, as are
  strings, quoted identifiers, dollar quotes, comments, an empty `{}` and
  any statement with a brace that never closes. Every client gets it, and
  DuckDB is untouched.

## 0.39.2 — 2026-09-18

- **The json modes emit a `VARIANT` or `JSON` cell as JSON.** The wire
  carries such a cell as JSON text, and `--mode json` / `--mode jsonlines`
  used to quote that text as a string, so `doc.patient.age` came out as
  `"43"` and a document as `"{\"a\":1}"` — JSON inside JSON, parsed twice
  on the other end. The text is now spliced in as the JSON it is: `43`, an
  object, an array, `true`, `null`. This is what `duckdb -json` does with a
  `JSON` column. A SQL NULL and a JSON null are both `null`; the wire and
  csv still tell them apart. The text is checked first, so a row is always
  well-formed: `NaN` and `Infinity`, which the engine's cast writes bare and
  JSON cannot say, stay the strings `"NaN"` and `"Infinity"`. The check does
  not recurse, so a document is spliced whole at any depth. A pretty-printed
  `JSON` column
  keeps its newlines in the engine; jsonlines is one record per line, so
  between tokens they become spaces. JSON nested inside a struct, list or
  map column is a string, as the wire holds it. The display modes are
  unchanged: a `VARIANT` string shows bare, a `JSON` column shows its text
  with the quotes, as DuckDB's own table does. csv and the wire are
  unchanged.

## 0.39.1 — 2026-09-18

- **A `VARIANT` string shows without its quotes in a table.** The wire
  carries a `VARIANT` cell as JSON text, and the display modes (box,
  markdown, line, list) used to print that text as it came, so a string
  read by path — `raw_request.requisitionNumber` — showed as
  `"L2605106156"`. A cell that is a JSON string now shows its content, the
  way a `VARCHAR` always has; a number, a boolean, a null, an object or an
  array is unchanged. Only the display modes do this: csv, json and
  jsonlines stay raw, so a program on the other end still tells 42 from
  "42", and the wire is untouched. Cast a path to `JSON` to see the quotes
  in a table.
- **The engine can be pinned to a harbor release.** `DUCKDB_ENGINE_RELEASE`
  names a release (`v0.39.0`) whose archive supplies `libduckdb` to
  `scripts/fetch-duckdb.sh`, CI and `Release.yml`, in place of the channel's
  current build; the CLI and headers still come from the channel. The
  channel is a moving pointer with no way to ask for an older build, and on
  2026-09-17 it moved to an engine whose v2 C API had been reworked
  (duckdb/duckdb#25751) and which this harbor cannot load. The repository
  variable of the same name holds the pin; clearing it returns to the
  channel.

## 0.39.0 — 2026-09-17

- **A `VARIANT` column arrives over HTTP as JSON text.** It used to arrive
  as the engine's display text — `{'method': POST}` — which no client could
  parse, and which printed the number 42 and the string "42" the same way.
  The encoder now casts each variant to JSON on the connection that produced
  it, so a document that entered as JSON leaves as the same JSON, and the
  schema line says `"encoding": "json"` instead of `"varchar-cast"`. JSON has
  no date or timestamp of its own, so those variant members arrive as strings
  and the column stays `"lossless": false`. `GEOMETRY` is unchanged. The
  README's known limitations now carry the measured list of where a
  `VARIANT` is not quite JSON, and the write rule that goes with it.
- **A deeply nested JSON document no longer takes the server down.** The
  engine recurses once per level when it builds a `VARIANT` from JSON, and
  on the default 2 MiB thread stack a document some 7,700 levels deep
  overflowed the executor and aborted the whole process. The threads that
  run SQL now have a 16 MiB stack, reserved rather than committed, which
  puts the edge past 60,000 levels.

## 0.38.0 — 2026-09-16

- **The engine comes from DuckDB's official nightly channel again.** For a
  month the channel shipped a library without the v2 C API, so harbor built
  its own from source at a pinned commit and shelved it on an `engine-<pin>`
  prerelease. The channel now ships the v2 API nightly, so that scaffolding
  is gone: `make fetch-duckdb`, CI and the release workflow all pull DuckDB's
  own `v2.0-cyanoptera` tarballs through one script, which refuses a library
  without the v2 API should the channel ever regress. Release archives
  bundle the build they were made with, as before.

## 0.37.0 — 2026-09-16

- **A `VARIANT` column backs up as JSON text, and comes back exactly when
  it entered as JSON.** Text could not carry one at all: the number 42 and
  the string "42" both print as `42`, and the reader hands every cell back
  as a string, so any table with a variant column was quietly written as
  parquet. It is now written as JSON — `42` for the number, `"42"` for the
  string, still greppable — and decoded on restore, which returns every
  value that entered as JSON with its inner types and nesting intact. The
  decode is an `UPDATE`, and DuckDB's `IMPORT DATABASE` accepts nothing but
  `COPY`, so it lives in a new `after.sql` beside `load.sql`: `restore` runs
  both, and a stock `duckdb` importing the directory by hand gets the JSON
  text and can run the second file itself. What JSON has no word for — a
  `DATE`, a `DECIMAL`, a `BLOB` put inside a variant from SQL — returns as
  JSON's nearest type, and the backup says so once per column, naming the
  types; `--strict` refuses instead, and `--format parquet` keeps them. A
  `VARIANT` nested inside a `STRUCT`, `LIST` or `MAP` still goes to parquet.
- The blank-record check, which decides whether a file is written with
  every value quoted, now looks at the file that will be restored rather
  than the one `EXPORT` wrote first.


- **A completion menu lets go of the statement it was opened for, however
  fast it was typed.** The menu stands down when a typed character can no
  longer extend the word it is completing, but it read only the first
  character of each batch of edits, and consecutive keystrokes reach the
  editor fused into one. Typing the tail of a statement at any speed
  therefore hid the boundary inside the batch: `show tab`, Tab, `les;`,
  Enter appended the highlighted `table` to a finished statement rather
  than running it. Every character in a batch is now read, so the word ends
  wherever its boundary falls.
- Records the vendored reedline copy as carrying three patches rather than
  two, with the upstream state of each and what un-vendoring will require.

## 0.36.2 — 2026-09-11

- **A summoned server no longer leaves while a client is still there.** The
  mooring that keeps a spawned server ashore was a connection that opened and
  never sent a request, and such a connection is reclaimed after sixty
  seconds — its silence is indistinguishable from an anonymous caller sitting
  on a descriptor. A repl whose human paused to think, or a backup between
  passes, therefore lost its berth and met `cannot reach harbor: No such file
  or directory` on the next statement. The mooring now asks `/ready` once and
  holds the answered connection, renewing every 240 seconds inside the
  server's 300-second idle clock, so the berth stays for as long as the
  client does. This reaches every client that summons a server: the repl,
  piped and `-c` scripts, `backup`, and `restore`.
- The lifecycle suite holds its mooring past the first-request timeout
  instead of for a few seconds, putting the clock that reclaims a silent
  connection inside what the test can see.

## 0.36.1 — 2026-09-11

- Keeps Windows' native canonical database paths for file access, server
  identity, and configuration. The `\\?\` prefix is hidden only when
  rendering paths in the banner and fleet display, preserving long-path
  support while keeping displayed paths readable.
- Adds a regression covering native canonical paths for existing and new
  databases under long directory names, plus Windows drive/UNC display cases.
  Release builds run the shared path tests on each supported platform.
- Sends the round-trip suite's large SQL fixtures through stdin so Linux
  argument-size limits cannot prevent the backup checks from running.
- Completes the 0.36.0 backup notes and corrects the description of config
  permission and symlink protections.

## 0.36.0 — 2026-09-11

- **A backup reads one snapshot from its first pass to its last.** `backup`
  may export a table again after scanning its first export for blank records
  or deciding that its types need another format. These passes previously
  used separate requests that could see different committed data. They now
  share one transaction. A session opened with `{"purpose":"backup"}` has no
  statement-idle timeout; it lives in a 60-second window that
  `POST /sql/sessions/<id>/renew` extends, and the client renews it from a heartbeat every twenty seconds,
  through SQL and file work alike. Renewals travel the control path, so a busy
  worker cannot starve them. Losing the lease aborts the backup, even
  mid-response, rather than resuming on a snapshot the first pass never saw,
  and an expired or released session cannot be revived. Ordinary sessions
  cannot be renewed; `/sessions` reports `renewable`. An older server without
  the route is refused with an explicit ask to upgrade.
- Backup loader rewriting handles quoted schemas and identifiers, apostrophes,
  and newlines using the shared SQL scanner. Loader filenames remain relative
  so a backup restores after its directory is moved. File scans use bounded
  buffers, and table-type metadata is indexed once for the rewrite passes.
- A table whose types cannot round-trip through either supported format is
  refused, including combinations of text-incompatible types and
  parquet-incompatible types. Failed exports remove their incomplete directory.
  Sequence counters remain nontransactional; exact alignment with exported
  rows requires quiescing sequence users.
- The backup heartbeat replaces the ordinary five-minute session ceiling,
  while the operator's statement timeout still limits each export statement.
- **One statement per request, decided before any of it runs.** A body with
  two statements used to run the leading ones and answer for the last; it is
  now refused with `400` and nothing executes.
- **A connection is replaced, not rolled back, before another caller sees
  it.** `ROLLBACK` undoes a transaction and nothing else — a `SET VARIABLE`,
  a temp table, and a `PREPARE` survived it onto the next request.
  Any statement that can leave such state, including one wrapped in
  `EXPLAIN ANALYZE`, now costs the worker a fresh engine connection, and a
  released session gets the same treatment. The parsed-statement cache goes
  with the connection.
- **Operator settings are locked once the berth is up.** Memory, threads and
  spill are fixed at `start`, and DuckDB's own `allowed_configs` and
  `lock_configuration` now enforce it — a wrapped `SET threads` or a `SET
  lock_configuration=false` is refused by the engine rather than by a keyword
  check. Other settings registered at startup remain changeable unless
  initialization imposed stricter locks. Load
  extensions whose settings must be tunable during `--init`; settings that
  arrive later are outside the allowed list.
- **Limits that answer instead of truncating.** A request body over the limit
  is refused with `413` on both endpoints, chunked or not, where before the
  excess was quietly cut off and parsed. The parsed-statement cache keeps at
  most 64 texts and 1 MiB of SQL, and does not retain a statement over 64
  KiB, so a one-off bulk `INSERT` runs but cannot pin every connection's cache.
- **`config.toml` writes take a lock.** Concurrent membership updates each
  land through a lock spanning read, modification, and atomic replacement.
  Temporary files use exclusive creation. On Unix, the lock file is opened
  without following symlinks, owned config directories are secured to `0700`,
  and foreign-owned directories, permission failures, and unsafe config files
  are refused. This does not reject symlinks throughout the entire config path.
- The Windows banner prints `C:\...` rather than the `\\?\C:\...` that
  `canonicalize` returns.
- New `regressions` suite, run on an isolated one-worker server so connection
  reuse is deterministic; `make test` now runs the workspace with all
  features in release mode.

## 0.35.0 — 2026-09-09

- Adds `harbor <db> backup [dir]` and `harbor <new.duckdb> restore <dir>`.
  A `.duckdb` file is only as portable as the engine that wrote it, so copying
  one is a snapshot, not a backup. `backup` writes the durable thing —
  `schema.sql`, `load.sql`, and one tab-separated file per table — greppable,
  diffable, and readable by anything.
- The dialect is three values and one escape: a bare `NULL` is a real null, a
  quoted `"NULL"` is the string, an empty field is an empty string. Quotes are
  always allowed and rarely required — the reader takes a bare field and a
  written `""` the same way, so a backup stays editable by hand.
- One exception, and it is a row of data rather than a matter of taste: a
  one-column table holding an empty string writes an empty LINE, and every CSV
  reader skips those — the row would not come back and nothing would say so.
  `FORCE_QUOTE` takes a column list rather than a predicate, so it is spent per
  FILE: a table whose export contains a blank record is written again with
  every value quoted, and no other file pays for it. The test is the failure
  itself rather than a proxy — a blank record between rows, walked
  quote-aware, so a blank line inside a multi-line value is left alone.
- The backup directory is self-contained. Each `COPY` names its file and
  nothing more, so it can be moved, renamed, copied to another machine or
  committed to a repo and still restore — an absolute path would have nailed
  it to the machine that wrote it.
- `restore` builds a **new** database and refuses one that exists, since a
  restore that can overwrite can be run at the wrong moment and destroy what
  it was meant to protect. It is also where `--block-size` applies, block size
  being fixed at creation. Both verbs act on contents rather than lifetime, so
  they stand alone and combine with no other verb. Retention, rotation,
  scheduling, compression and remote targets stay out: cron, a filesystem and
  `rsync` already do those.
- `backup` takes `--format tsv|parquet` and `--strict`. Neither format holds
  every type and the holes are not the same shape: text loses a `UNION`'s tag
  (the restore then refuses) and retypes a `VARIANT`'s contents (it does not),
  while parquet refuses a negative `INTERVAL` outright and normalises a
  `TIMETZ` to UTC — `12:00:00+02:30` returns as `09:30:00+00`, the same
  instant, a different value, and nothing said. Each hole is the other
  format's solid ground, so a table the chosen format cannot carry is written
  in the other one and named out loud, with `load.sql` recording the format
  per table. `--strict` refuses instead of swapping. The rule under all three
  is the same: never write something that will not come back.
- New `roundtrip` suite: back up and restore every type in the shared corpus,
  a schema of constraints, indexes, views and sequences, the strings that
  attack the format, and a seeded fuzz of random tables — then attach both
  databases and ask DuckDB whether anything differs, rather than reading the
  export back and finding the writer agrees with the writer.
- `--block-size` now also works on the summon — `harbor <db> --block-size 64k
  -c "..."` shapes the database that call creates, instead of the size being
  reachable only through an explicit `start`. A size that reached nothing,
  because a server was already up or the file already existed, says so.

## 0.34.0 — 2026-09-09

- **`USE` outside a session is now refused instead of silently discarded.**
  It sets the current database on the connection it runs on; that connection
  is pooled, and a request carries exactly one statement, so nothing could
  ever follow it there. It reported success and was thrown away, every time.
  The refusal names the session that does persist (`POST /sql/sessions`) and
  the qualified names — `database.schema.table` — that need no session at all.
  Inside a session `USE` is unchanged.
- Adds `--block-size` and the matching `block-size` config key: the block size
  for a database the call CREATES, from 16k to 256k. A block is both the unit
  of allocation and the window a column's compression works in, and the
  default suits few large tables rather than many small ones — a dozen
  near-empty tables cost 9.5 MiB at 256k. It travels as an open-time option
  because a later `SET` cannot reach it.
- `default_block_size` under `[settings]` is now dropped and reported rather
  than emitted as a `SET` that succeeds and changes nothing, and a server
  asked for a block size the file does not have says so — block size is fixed
  when a database is created, so a config naming another one was a wish that
  read like a setting.
- Fixes statements that expand to a group of statements — `COPY FROM DATABASE`
  and `IMPORT DATABASE` — which could not run at all. The result schema was
  read before anything had stepped, and for these the result-producing member
  of the group is not prepared until something does. A failing group statement
  now reports its own complaint rather than the readiness error that was only
  how it surfaced.
- The `unit` suite runs `--workspace --all-features`. `default-members` and two
  off-by-default features had been hiding 101 tests — all of `crates/justhttp`
  and all of the config reader — which compiled but never ran.

## 0.33.1 — 2026-09-06

- A verb typed without a database (`harbor restart`) now names the attached
  databases in its redirect, with the command spelled out for the first one,
  instead of showing only the file form.

## 0.33.0 — 2026-09-06

- Makes `autostart` a service: the login item is loaded the moment it is
  installed, so the server starts now under launchd or systemd, at every
  login, and again after a crash (`KeepAlive` on failure only; systemd
  `Restart=on-failure`). A clean `stop` stays stopped until the next login.
- Adds `restart`, which bounces a database under its login item with a fresh
  read of config.toml, and `autostart off`, which drops the login item and
  leaves a running server alone (`autostart off stop` takes both down).
- Stops a plain `start` or `stop` from removing the login item; only
  `autostart off` and `detach` do.
- Refuses start options on `autostart` and on a login item's `restart`,
  pointing at the `[connection.<name>]` entry a login item actually reads.
- Sends the login item's output to the berth's log under `runtime/log/`.
- Makes `start` at a terminal bring the server up in the background and
  return — under the database's login item when it has one, otherwise as a
  detached process that runs until `stop`. The prompt-as-server helm is
  gone: a prompt is what bare `harbor <db>` opens. Headless `start` still
  serves in place until SIGTERM, and `--foreground` asks for that shape at a
  terminal. `stop` returns once the server has actually gone quiet, so
  `stop` followed by `start` — by hand or inside `restart` — always meets a
  free database.
- Names a database by the config key that lists it, wherever a name is
  derived — the login item, the stopped row, `attach`, `detach` — so an entry
  such as `[connection.warehouse]` pointing at `inventory.duckdb` never grows
  an `inventory` twin and never boots without its settings.
- Fixes the systemd unit, which ordered itself after the target that wants
  it — a cycle systemd resolves by dropping a job — and sends the unit's
  output to the berth's log like the LaunchAgent does.
- Carries `HARBOR_HOME` and the XDG home variables into the login item when
  the installing shell had them set, so the manager's server and the CLI
  agree on where the socket is.
- Makes `start` at a terminal a success when the server is already up, the
  way `systemctl start` treats an active unit; headless it stays a refusal,
  so a manager or a spawn never reads a clean exit as a server it did not
  get. A running server announces the config key that lists it in `/info`,
  so its bare name is the same word whether it is running or stopped.
- Unloads a login item whose server fails to come up within 15s, instead of
  letting the manager retry it every ten seconds until logout; the item stays
  registered for the next login and the next `start`.
- Lists attached databases that are not running as dimmed `stopped` rows in
  bare `harbor`, with the file path and whether a login item will bring them
  back. Their names and footnote numbers now resolve everywhere a running
  one's do, so `harbor medlabs start`, `harbor 2 autostart` and a bare
  `harbor medlabs` all work with nothing running.

## 0.32.1 — 2026-09-05

- Shows the installed Harbor CLI version as a caption joined to the fleet
  table printed by bare `harbor`.
- Adds a `VERSION` column with the version reported by each running database
  server, making processes that still need a restart immediately visible.

## 0.32.0 — 2026-09-04

- Adds generated-column metadata to `/catalog` through `generated` and
  `generationExpression` fields.
- Documents `httpfs` initialization using Harbor's boot-SQL support.

## 0.31.1 — 2026-09-03

- Publishes synchronized Harbor patch packages and versioned installation
  examples for the 0.31 release line.

## 0.31.0 — 2026-09-03

- Makes full `/catalog` row counts exact by counting every table in one
  ordered `UNION ALL` query.
- Adds `/catalog?style=lite` for fast inventory requests without counts,
  columns, constraints, indexes, DDL, or sequences.
- Keeps full catalog responses as the complete schema document with exact
  database and WAL sizes.

## 0.30.0 — 2026-09-03

- Restricts Harbor's TCP listener to IPv4 loopback.
- Aligns configured addresses, sidecar discovery, and `/info` with the
  loopback-only TCP contract.

## 0.29.0 — 2026-09-03

- Establishes the current direct Harbor connection model across the CLI,
  server, clients, configuration, installers, tests, and documentation.
- Simplifies local socket and HTTP connection setup to the endpoint alone.

## 0.28.3 — 2026-09-03

- Fixes supervised TCP startup when endpoint settings are supplied entirely
  by configuration and the service environment.

## 0.28.2 — 2026-09-03

- Prevents a systemd or launchd login item from disarming itself when it
  starts its configured database.
- Moves the test sandbox under `/tmp` so Unix socket paths fit platform limits.

## 0.28.1 — 2026-09-02

- Shows the server's resolved name in the interactive prompt.
- Improves errors when a bare word does not identify a running database.

## 0.28.0 — 2026-09-02

- Adds a URL column to the fleet display whenever a database exposes a TCP
  listener.
- Allows running databases and lifecycle verbs to resolve socket paths, URLs,
  unique names, or fleet footnote numbers.
- Adds consistent uninstall support to release installers.

## 0.27.0 — 2026-09-02

- Adds an optional TCP listener alongside the always-present Unix socket.
- Teaches the HTTP server to operate multiple listeners and records which
  transport accepted each request.
- Reports the active TCP port through `/info` and packages ICU and JSON with
  the DuckDB engine.

## 0.26.1 — 2026-09-02

- Gives an empty fleet the same table frame and headers as a populated fleet,
  followed by a clear “Nothing running” status.

## 0.26.0 — 2026-09-02

- Adds typed per-database configuration for memory, threads, workers,
  statement timeout, temporary storage, access mode, logging, and TCP
  exposure.
- Adds verbatim boot SQL and arbitrary `[connection.*.settings]` values for
  DuckDB and extension configuration.
- Makes explicit starts and autostart consistently honor saved settings while
  on-demand local opens remain socket-based.

## 0.25.0 — 2026-09-02

- Adds the order-independent `start`, `stop`, `attach`, `detach`, and
  `autostart` lifecycle grammar.
- Reports a server's ephemeral lifetime through `/info` so restarts preserve
  how it was started.
- Cleans up obsolete lifecycle surfaces and standardizes current command
  vocabulary across code, installers, and documentation.

## 0.22.0 — 2026-09-01

- Preserves `TIMETZ` UTC offsets in ISO output.
- Preserves union tags at every nested depth.
- Encodes DuckDB's `24:00:00` end-of-day value without wrapping it to
  midnight.
- Single-sources the DuckDB engine pin and hardens release artifact checks.

## 0.21.0 — 2026-09-01

- Moves execution to DuckDB's first-party v2 C API with generated bindings,
  chunk-level streaming, cached parsed statements, and direct value encoding.
- Pipelines fetching and encoding for substantially faster large results and
  compresses responses with negotiated Zstandard.
- Adds support for nanosecond time values, textual `VARIANT` and `GEOMETRY`,
  and structurally encoded tuples.
- Loads `libduckdb` only when database work begins and moves Harbor into the
  shared monorepo under `harbor/`.

## 0.20.0 — 2026-09-01

- Makes one Harbor binary both the command-line client and database server.
- Adds refcounted ephemeral servers that remain available while clients are
  connected and retire after the final client leaves.
- Derives server identity from the database path and removes registry and
  timer-based lifecycle state.
- Loads the DuckDB engine on demand rather than at process launch.

## 0.19.1 — 2026-08-31

- Makes stopping an already-stopped known database idempotent.
- Strengthens the distinction between configured database names and direct
  file paths throughout the prompt and lifecycle commands.
- Adds operator stop holds, centralized sidecar discovery, clearer fleet
  footnotes, and expanded lifecycle tests.

## 0.19.0 — 2026-08-30

- Treats a configured name as a persistent service and a direct path as a
  temporary database session.
- Makes temporary databases identify themselves clearly in fleet output.
- Keeps autostart an explicit property of configured databases.

## 0.18.0 — 2026-08-30

- Reworks fleet management around the `show`, `start`, `forget`, and `doctor`
  commands with shared discovery and plain-language output.
- Expands `/catalog` with database sizes, table row statistics, and engine DDL,
  plus a lightweight inventory form.
- Introduces a shared common crate for configuration, paths, state, and UI
  rules used across Harbor clients.
- Moves release installation into the current user's home on every platform.

## 0.15.0 — 2026-08-29

- Hardens HTTP parsing with bounded request heads and bodies, framing checks,
  connection deadlines, and recoverable listener errors.
- Closes statement-smuggling edge cases involving carriage returns and dollar
  signs inside identifiers.
- Improves `/catalog` index metadata, nested union honesty, map typing, and
  `FLOAT` rendering.
- Strengthens shutdown, readiness, terminal safety, and hostile-input test
  coverage.

## 0.14.0 — 2026-08-27

- Moves user configuration to `~/.config/harbor/config.toml` and runtime fleet
  state to its protected `runtime/` directory.
- Ships an example configuration file in release archives.
- Adds bootstrap installers that migrate a stopped earlier installation and
  enforce private directory permissions.
- Makes database creation explicit and improves Pilot conflict reporting.

## 0.13.2 — 2026-08-26

- Improves Pilot REPL lifecycle handling and completion behavior.
- Makes stress tests tolerate bounded 503 load shedding.
- Reconciles platform documentation with the supported release targets.

## 0.13.1 — 2026-08-24

- Closes the completion menu when statement punctuation or another word
  boundary is typed, preventing stale suggestions from consuming Enter.

## 0.13.0 — 2026-08-24

- Adds a per-connection prepared-statement cache and batches the HTTP response
  hot path, nearly doubling small-statement throughput in benchmarks.
- Reduces allocations in request parsing, result encoding, and response
  framing.
- Adds `version`, `-V`, and `--version` commands to Harbor and Pilot.

## 0.12.0 — 2026-08-24

- Replaces the external HTTP implementation with Harbor's first-party
  `justhttp` HTTP/1.1 crate.
- Preserves wire behavior while matching prior throughput and removes the
  patched HTTP dependency from the tree.

## 0.11.1 — 2026-08-17

- Makes the release installer safe when invoked through `sudo`, keeping
  user-space files owned by the invoking user.
- Updates artifact actions used by the release workflow.

## 0.11.0 — 2026-08-17

- Consolidates the server and CLI into the standalone Harbor binary with
  `wire` and `pilot` companion crates.
- Builds against the official DuckDB 2.0 development engine and removes
  extension-era build machinery.
- Adds self-contained release archives for macOS ARM64, Linux AMD64/ARM64,
  and Windows AMD64/ARM64.
- Adds reproducible engine fetching, release smoke tests, and a separate full
  CI suite.

## 0.9.1 — 2026-08-15

- Adds unique constraints to the `/catalog` schema document.

## 0.9.0 — 2026-08-15

- Adds `GET /catalog` as one structured call for tables, columns, constraints,
  indexes, sequences, views, macros, and attached catalogs.
- Adds scripted, repeatable release assembly for all supported native targets.

## 0.8.2 — 2026-08-13

- Replaces the static health response with `/ready`, which verifies the
  database through the normal query path.
- Adds negotiated single-document JSON responses alongside streaming NDJSON.
- Adds leased sessions for multi-request transactions and cancellation for
  running or timed-out statements.
- Updates the release matrix to macOS ARM64, Linux AMD64/ARM64, and Windows
  AMD64/ARM64.

## 0.8.1 — 2026-08-12

- Adds an interactive REPL that becomes the default at a terminal.
- Adds opt-in request logging with status and end-to-end request duration.
- Improves standalone extension discovery and compatibility across DuckDB
  builds.
- Adds one concise startup summary table and fixes untyped `NULL` decoding on
  DuckDB v2.

## 0.7.0 — 2026-08-11

- Introduces Harbor's HTTP `/sql` server, readiness checks, lifecycle commands,
  graceful checkpointing, and keep-alive connections.
- Adds streaming DuckDB result encoding, transaction cleanup after abandoned
  streams, and broad protocol and resilience tests.
- Publishes native release binaries for Linux AMD64/ARM64, macOS AMD64/ARM64,
  and Windows AMD64.
