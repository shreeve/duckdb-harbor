<p align="center">
  <img src="duckdb-harbor-social.png" alt="DuckDB Harbor" width="600">
</p>

# duckdb-harbor

> **Many clients, one DuckDB, over plain HTTP. `POST` a statement, read NDJSON
> back.**

Only one process can access a DuckDB file at a time. Harbor fixes that: it
puts a small server in front of the file so all your apps can share it — and
it feels exactly like the duckdb shell, except a database can also serve.

DuckDB Harbor is `harbor`, one small Rust binary with one grammar:

```console
$ harbor                       # what's running
$ harbor mydata.duckdb         # open it — REPL, or -c "SQL", or stdin
$ harbor mydata.duckdb start   # bring it up in the background, until you stop it
```

`harbor mydata.duckdb` is the duckdb-shell muscle memory, kept: a REPL with
highlighting and completion, `-c` for one-shots, stdin for scripts. The
difference is what happens behind it — if nothing serves the file yet, a
server is spawned for it, and every other client of the same file joins that
server instead of hitting "database is locked". The two lifetimes, in one
breath: **bare, the server is everyone's — it lives while anyone is
connected; `start`, the server is yours — it lives until you stop it.**

## The Elevator Pitch

Two files. That's the entire install.

The library is DuckDB — all of it, one dynamic library, vanilla, compiled and
shipped by the DuckDB team. We never patch it, fork it, or wrap it in
bindings. Version hop = swap the file.

The binary is harbor: one 2.2MB executable that is both sides of the
conversation. As a server it loads libduckdb and serves your database over
HTTP, on a Unix socket or TCP. As a client it connects to any harbor and
gives you a modern shell — syntax highlighting, completion, history — in
place of the DuckDB CLI.

You never choose which one you're running. `harbor mydb.duckdb` connects if
the database is already being served, and spawns a server and connects to it
if it isn't. Your connection is the server's lifeline: the database stays
served while anyone is connected — a second client makes it two, your exit
makes it one — and when the last client leaves, the server checkpoints and
departs. Nothing to daemonize, nothing to clean up. (Want it to outlive its
clients? `harbor mydb.duckdb start` — then it's yours until you stop it.)

While it's up, anything that speaks HTTP can query it: curl, your app,
another harbor.

`harbor` by itself shows what's being served.

Zero-config by default. No drivers, no ORM, no fleet manager. Built directly
on DuckDB's new v2 C API — the API of the 2.0 line — so it's smaller, faster,
and simpler than everything it replaces.

If it can speak HTTP and parse JSON, it can query your database.

```console
$ curl -s 127.0.0.1:9495/sql -H 'Content-Type: application/json' \
       -d '{"sql":"SELECT id, total FROM orders LIMIT 2"}'
{"type":"schema","columns":[{"name":"id","duckdbType":"BIGINT","lossless":true},
                            {"name":"total","duckdbType":"DECIMAL(10,2)","lossless":true,
                             "decimal":{"width":10,"scale":2}}]}
{"type":"row","values":[1,"19.99"]}
{"type":"row","values":[2,"4.50"]}
{"type":"end","rowCount":2,"timeMs":3}
```

One `schema` message, one `row` per row, one `end`. Rows go out as DuckDB
produces them, so a client can start on row one while the server is still
producing the last one.

Nine routes. That is the whole surface — two of them for queries, three so a
transaction can outlive one request, one to stop a statement that is running,
one to read the schema without asking five questions, one that says who a
server is, and one graceful shutdown route:

```
GET  /ready                can this server answer a query?
POST /shutdown             drain, checkpoint, and stop

GET  /info                 identity — database path, versions, pid, uptime,
                           and the live client count
GET  /catalog              everything about the database in one stable JSON
                           document — schema, sizes, exact row counts, DDL
                           (?style=lite for the count-free inventory)

POST /sql                  run one statement, stream the result as NDJSON
                           (Accept: application/json for one document instead)
POST /sql/sessions         take a connection and hold it, for a transaction
GET  /sql/sessions         list ALL open sessions — who holds each, how long
DELETE /sql/sessions/<id>  give that one back
POST /sql/sessions/<id>/renew  renew a backup lease
DELETE /sql/queries/<id>   stop a statement the caller named when it sent it
```

Three alias spellings stay served beside the canonical routes:
`POST /sql/sessions/new`, `GET /sessions`, and `DELETE /shutdown`.

`POST /sql` streams by default. Send `Accept: application/json` and the same
result comes back as one document instead:

```json
{"ok":true,
 "columns":[{"name":"id","duckdbType":"INTEGER","lossless":true}],
 "data":[[1],[2]],
 "rowCount":2,
 "timeMs":3}
```

Same columns, same values, same encoder — only the framing differs. It is worth
asking for when the result is small and a single `JSON.parse` is simpler than
reading lines; it is the wrong choice for anything large, because a JSON
document is not valid until its last byte, so nothing can be flushed as it is
built. Harbor holds at most 32 MiB for one and refuses past that with a `406`
naming NDJSON as the remedy. Streaming has no such limit.

The one thing one-shot does better: since nothing has been sent when the last
row lands, a failure is still a real status code. The same query that streams a
`200` with an `{"type":"error"}` line at the end answers `400` in this shape.

The stream compresses on request — `Accept-Encoding: zstd`, the standard
coding browsers, newer curl, and Node/Bun offer on their own. The wrapped
bytes are the identical NDJSON: a 5M-row integer result measures 161MB
plain and 1.1MB as zstd, and when the query does any real work the
compression rides the writer thread for free. Anything else — gzip
included, its encoder would throttle the stream — gets identity, and
`curl -H 'Accept-Encoding: zstd' ... | zstd -d` recovers the stream
byte-for-byte.

`/ready` normally runs `SELECT 1` through an ordinary executor and answers `200
{"status":"ready"}` or `503`. Under sustained worker saturation, the dedicated
probe lane asks the control connection instead, so a load balancer can still
distinguish busy from dead. It is not a process-liveness check: a process can be
running while its database path is broken. Verdicts are cached for one second,
so polling costs at most one probe query per second however often it is asked.

## Stopping a statement

A statement that has entered DuckDB does not come back until it is done, and
harbor runs a small, bounded number at once. So a query nobody wants any more
is not a slow request — it is a connection out of service, and enough of them
are the whole server.

Name a statement when you send it, and you can stop it:

```console
$ curl -s 127.0.0.1:9495/sql -H 'Content-Type: application/json' \
       -d '{"sql":"SELECT count(*) FROM huge","queryId":"report-7"}' &
$ curl -s -X DELETE 127.0.0.1:9495/sql/queries/report-7
{"cancelled":true}
```

When cancellation lands before streaming begins, the statement answers `499`
with `{"code":"cancelled"}` — nginx's code, because there is no standard one
for "the caller withdrew" and neither `400` nor `500` is true. If a streaming
response already began with `200`, cancellation arrives as its final NDJSON
error event instead; an HTTP status cannot be changed after its headers were
sent. Cancelling something that already finished is `{"cancelled":false}`, not
an error: by the time a Stop button is pressed, the query it refers to may well
be over.

The id is chosen by the caller rather than issued by harbor, and it has to be:
the response does not begin until the statement is streaming or done, so an id
in the reply would arrive too late to be any use. It is refused with a `409`
while a statement of that name is already running, so two live queries can
never share one name and make a cancel a coin flip.

**A deadline is the backstop.** `{"timeoutMs": N}` on a request, or
`HARBOR_STATEMENT_TIMEOUT_MS` for a whole deployment, stops a statement without
anyone having to ask. There is no default, deliberately: harbor streams
300,000-row results and is used for queries that take minutes on purpose, so a
default deadline would break correct programs to catch incorrect ones. With no
deployment cap, zero on a request means no limit. When a deployment cap is set,
it is a hard ceiling: a request may ask for less time, but neither a larger value
nor zero can opt out of the operator's limit.

Explicit cancellation remains reachable when every executor is inside a long
statement: after sustained saturation, a connection-free probe lane accepts
query cancellation, session release, readiness, and inspection requests. The
reaper is the independent backstop. It runs on its own thread and never touches
HTTP, so deadlines are still enforced if no cancellation request arrives or a
client disappears. If a deployment's worry is runaway queries rather than
impatient users, set `HARBOR_STATEMENT_TIMEOUT_MS` or
`--statement-timeout <duration>`.

Two smaller things follow from the same machinery. Releasing a session whose
statement is still running stops it — `{"released":false,"cancelling":true}`
— and the connection comes back on a reaper tick after execution stops. And a
lease that blows its TTL while busy is reclaimed: the lease that most needs
taking back is the one wedged inside a runaway statement.

Cancelling a statement inside a transaction aborts that transaction, exactly as
it does in Postgres. Harbor does not paper over it — the next statement gets
`Current transaction is aborted (please ROLLBACK)` until you do. Rolling back
silently would let the statement after a cancellation commit in autocommit
under a client that still believed it was in a transaction. The rule has no
exceptions: a 499 inside a transaction means that transaction is over. A
cancel that lands before the statement has begun to execute — while an object
or array parameter is still being bound, or before an executor has picked the
statement up — answers the same 499 and harbor leaves the transaction aborted
itself, so a client that carries on cannot commit a transaction with a
statement missing. A `COMMIT` sent to an aborted transaction rolls it back. In
autocommit there is no transaction to abort, and the next statement runs.

## Transactions

A transaction lives on a connection and HTTP requests do not, so one request
per statement means no transaction can span two. A session bridges that: a
connection pinned to you until you release it or its lease expires. COMMIT and
ROLLBACK end the transaction; DELETE releases the session.

```console
$ sid=$(curl -s -X POST 127.0.0.1:9495/sql/sessions | jq -r .sessionId)
$ post() { curl -s 127.0.0.1:9495/sql -H 'Content-Type: application/json' -d "{\"sql\":\"$1\",\"sessionId\":\"$sid\"}"; }
$ post "BEGIN"
$ post "INSERT INTO orders (total) VALUES (19.99) RETURNING id"
$ post "INSERT INTO order_items (order_id, price) VALUES (1, 19.99)"
$ post "COMMIT"
$ curl -s -X DELETE 127.0.0.1:9495/sql/sessions/$sid
```

This is PgBouncer's transaction pooling, or ActiveRecord checking a connection
out of its pool — with an HTTP request where they have a socket and a thread.
The session rules are:

**Sessions draw from their own connections.** `HARBOR_POOL_SIZE` (default 16)
is opened at load and split: the workers take theirs, sessions get the rest. A
pool serving both would run out of workers the moment enough clients held
transactions open, and then answer nothing at all. With none free, opening a
session is a `503` with `Retry-After` — queries keep working throughout.

**Ordinary sessions have a fixed deadline.** Ask for a lifetime with
`{"ttlMs": N}`; Harbor caps it at five minutes and returns the granted lifetime
alongside a thirty-second idle timeout. Running SQL does not count as idle,
but the fixed deadline applies even while a statement is running. Ordinary
sessions cannot be renewed.

**Backup sessions renew their deadline.** Open one with `{"purpose":"backup"}`
and require `"purpose":"backup"` in the response (older servers do not support
this policy). Its `ttlMs` is a renewal window, default and maximum 60 seconds;
`idleTtlMs` is zero. Send `POST /sql/sessions/<id>/renew` well before each deadline
(the CLI uses 20-second intervals). Successful renewal returns `{"renewed":true}`
and starts a fresh window, even while SQL is running. Heartbeats replace both
the ordinary five-minute ceiling and the thirty-second statement-idle timeout.
Expired or released leases return `404` and cannot be revived; attempts to renew
ordinary leases return `400`. `/sessions` reports `renewable` and `expiresInMs`.
Renewals use the control path so a busy SQL worker cannot block them indefinitely.
Custom shorter renewal windows must allow for network and scheduling delays,
including up to five seconds before the control lane activates for forwarded
lease work; the CLI uses the full 60-second window.

**Expired sessions are reclaimed.** If a client disappears, its ordinary
session expires at its fixed deadline or idle timeout; a backup session expires
when its renewal window runs out. For either kind, Harbor cancels outstanding
SQL, then rolls back any open transaction and reclaims the connection once
execution stops. Explicitly releasing a session also rolls back an open
transaction. Before reuse, Harbor replaces the engine connection, clearing
temporary tables, variables, prepared statements, and connection-local
settings. These remain available between requests within the same live session.
One-shot requests also discard local state before the next caller; use a
session whenever later statements depend on it.

**One statement at a time.** A second statement sent while the first is running
gets a `409`: a transaction is a sequence, and two of them interleaving inside
one is something no client could reason about.

`GET /sessions` shows what is held — age, idle time, statements, whether a
transaction is open — and the connection accounting behind it. Free plus live
plus in-flight always equals total; `balanced` is that checked at the moment
you asked. A pool leaks connections silently and the symptom shows up weeks
later as "everything hangs", so the arithmetic is worth being able to read.

Note that DuckDB resolves write conflicts optimistically: two transactions
touching the same row do not queue, the second is refused the moment it writes.
The answer is to run the transaction again, which is what `rip/db` does for
you.

## Brace expansion

A statement can carry the shell's `{a,b}`, and Harbor expands it before the
engine sees it. It was made for reaching into a `VARIANT` several fields at
a time without repeating the path:

```sql
select
  id,
  raw_request.{
    requisitionNumber,
    visitDate,
    patient.{
      lastName,
      firstName,
    },
  },
from
  orders
where
  raw_request.patient.lastName ilike 'morel'
;
```

is what the engine runs as

```sql
select id, raw_request.requisitionNumber, raw_request.visitDate,
           raw_request.patient.lastName, raw_request.patient.firstName,
  from orders where raw_request.patient.lastName ilike 'morel';
```

Items are separated by commas. Whitespace alone separates them too, so the
comma is never required, only clearer. A group nests. It can sit
anywhere in a term — `orders_{2025,2026}`, `{first,last}Name`,
`r.{a,b}::VARCHAR` — and two groups in one term multiply, as in a shell:
`r.{a,b}.{x,y}` is four paths. The alternatives are joined with `, `, which is
what a select list, a `FROM` list and an argument list all take.

A struct literal is DuckDB's own use of braces, and it always carries a lone
`:` at its top level — never the `::` of a cast — so `{'a': 1}` is left as it
came, and so is an empty `{}`. Strings, quoted identifiers, dollar quotes and
comments are never touched. A brace that never closes leaves the statement as
it came, and the engine reports the syntax error at it.

The expansion happens in the server, once, for every client: the REPL,
`curl`, a Rip app. `EXPLAIN` and the engine's error messages show the
expanded statement. DuckDB itself is untouched; it only ever receives plain
SQL.

Groups multiply, so a short statement can stand for an enormous one: thirty
two-item groups are a billion alternatives. An expansion that would write or
re-read more than 16 MiB of text is refused with a `400` before anything
runs.

## The whole database, one call

`GET /catalog` answers what a client would otherwise ask in a dozen queries:
every table with its columns, primary key, unique constraints, foreign keys,
indexes and sequences — including whether a column is generated and its
generation expression — plus its exact row count and the engine's own `CREATE
TABLE` rendering per table, and the database and WAL file sizes in exact bytes
at the top:

```json
{"harborVersion":"0.32.1","duckdbVersion":"v2.0.0-dev83323",
 "databaseSizeBytes":12582912,"walSizeBytes":0,
 "tables":[{"name":"orders","schema":"main","rowCount":300000,
            "columns":[{"name":"id","type":"BIGINT","notNull":true,
                        "default":null,"generated":false,
                        "generationExpression":null,"primary":true},…],
            "primaryKey":["id"],
            "ddl":"CREATE TABLE orders(id BIGINT PRIMARY KEY, …);"}, …],
 …}
```

The document is stable — same database, same bytes — so clients can diff it.
A client that only wants an inventory asks `GET /catalog?style=lite` and gets
the versions, the sizes, and `{name, schema}` per table: what exists, without
counting it or describing how it is built, at a fraction of the work and bytes.
An unknown style value is a loud `400`; unknown parameters pass.

## Get it running

One binary, ready without configuration. The client half never touches DuckDB —
the engine (`libduckdb`) loads on demand, only when this process is the one
serving a file, so the same 2.2MB `harbor` is a pure protocol client on
machines that never host a database. `make fetch-duckdb` pulls DuckDB's
official artifacts into `~/.duckdb/cli/2.0.0/`, one of the places harbor
looks at runtime; then:

```console
$ make fetch-duckdb            # libduckdb + duckdb CLI -> ~/.duckdb/cli/2.0.0/
$ make harbor                  # -> target/release/harbor (no engine needed to build)
$ harbor mydata.duckdb
mydata>
```

`make bootstrap` does the whole thing in one shot — fetch the engine into
`~/.duckdb`, then build and install `harbor` into `~/.local/bin`. No step
needs root.

The engine `fetch-duckdb` pulls is DuckDB's official nightly of the 2.0
branch — the latest green build, which is a moving target by design. The
release archives below bundle the engine they were built with, so a release
is reproducible; a local fetch is deliberately current. The script refuses a
library that lacks the v2 C API harbor binds, before it installs anything,
since harbor would refuse it at dlopen.

`DUCKDB_LIB_BUILD` names the build of `libduckdb` to fetch. `latest`, the
default, is the channel's current build. Anything else is a DuckDB build by
name — `DUCKDB_LIB_BUILD=alpha42289 make fetch-duckdb` for
`v2.0.0-alpha42289` — taken from the `engine-<build>` release of this
repository, which holds that one library for each platform; the CLI and
headers still come from the channel. The channel cannot serve a build by
name, so an engine release is where a fixed one lives when the channel moves
to an engine harbor cannot load — as it did when the v2 C API was reworked
(duckdb/duckdb#25751). The script checks that the library it got says it is
the build it was asked for. The repository variable of the same name sets the
build for CI and the release builds; `latest`, or clearing it, returns to the
channel. Set it locally to match while the variable names a build, or
`make fetch-duckdb` refuses the channel's engine and installs nothing.

No toolchain? One command installs the latest release — it picks the right
archive for the platform, verifies its sha256 against the published checksums,
and installs `harbor` into `~/.local/bin` with `libduckdb` in `~/.local/lib`
(override with `BIN=...` `LIB=...`):

```bash
# macOS and Linux
curl -fsSL https://raw.githubusercontent.com/shreeve/duckdb-harbor/main/install.sh | bash

# Windows
irm https://raw.githubusercontent.com/shreeve/duckdb-harbor/main/install.ps1 | iex
```

Uninstall with `... | bash -s -- --uninstall` — the binary and `libduckdb`
go; your databases, state, and config stay.

Installed, it updates itself. `harbor update` runs that same installer over
the binary that ran it, so a copy outside `~/.local` updates in place (with
`sudo` in front when it is system-wide), and `harbor update 0.42.0` picks a
release, older or newer. A running server keeps the code it started with
until it is restarted, so the update ends by naming every server still on the
old version and the `restart` that brings each forward; `--restart` runs
them, and `--check` only says whether a newer release exists. Headless, as
from cron, `--restart` restarts only servers with a login item, since a
hand-started server's restart would serve in place, as `start` does there.

Nothing there asks for root. `~/.local/bin` is where the XDG base directory
spec puts user executables; Debian and Fedora already have it on `PATH`, macOS
does not, and the installer says so rather than putting binaries somewhere you
cannot see. A system-wide install is `BIN=/usr/local/bin LIB=/usr/local/lib`
with `sudo` in front of the whole command — the installer never escalates on
its own. On Windows the binary lands in `%LOCALAPPDATA%\Programs\harbor\bin`,
which the installer adds to your user `PATH`.

Pin a version with `... | bash -s v0.32.1` (or `-Tag v0.32.1` on Windows). Each
[release](https://github.com/shreeve/duckdb-harbor/releases)
ships one self-contained archive per
platform (osx-arm64, linux-amd64, linux-arm64, windows-amd64, windows-arm64):
harbor and the exact DuckDB shared library it was tested against. Unix
archives carry `bin/`, `lib/` and `install.sh`; Windows archives put
`duckdb.dll` beside the executable and run in place.

On Windows, banners and fleet displays omit the `\\?\` prefix for readability.
File access, configuration, and `/info` keep native canonical paths, including
that prefix where needed for long paths.

Linux archives are built on Ubuntu 24.04 and need glibc 2.39 or newer; on an
older distribution, build from source with `cargo build --release`.

cmd.exe does not treat single quotes as quoting, so a curl example written for
a Unix shell sends its body starting with a literal `'` there. Put the JSON in
a file and pass `-d @query.json`, which reads the same in every shell, or
double-quote the body and escape the quotes inside it. In PowerShell, `curl`
is an alias for Invoke-WebRequest; call `curl.exe`.

### The two lifetimes

**`harbor <db.duckdb>` — the server is everyone's.** On a terminal it is the
REPL — highlighting, completion (Down on the live line or Ctrl-Space lists,
Tab accepts; Up and Down are history everywhere else), the duckdb-shell dot
commands. With `-c` or stdin it runs statements and exits. Either way, if nothing serves the file
yet, a server is spawned behind the scenes: detached, refcounted, alive while
anyone is connected. Every client holds one silent connection for its
lifetime, so a human thinking at a prompt counts as presence; when the last
client leaves, the server drains, `CHECKPOINT`s, sweeps its socket, and exits
a few seconds later. A second `harbor` on the same file — any spelling of the
same path — joins the same server instead of reporting "database is locked".

**`harbor <db.duckdb> start` — the server is yours.** No refcount: it lives
until you stop it. At a terminal the server comes up in the background and
`start` returns — under the database's login item when it has one, so the
session manager owns it from the first second, otherwise as a detached
process that `harbor <db> stop` ends. Headless, meaning no terminal on
stdin, it serves in place until `SIGTERM`, which is the shape launchd,
systemd and a spawn want; `--foreground` asks for that shape at a terminal,
to watch a server work. Either exit is clean — drain, `CHECKPOINT` so the
next open never replays a WAL, socket swept.

**`harbor <db.duckdb> autostart` — the server is the session manager's.**
harbor never becomes a supervisor; it hands exactly this `start` to one that
already exists. On macOS it writes a LaunchAgent and loads it, on Linux a
systemd user unit, enabled and started. The server comes up now, at every
login, and again after a crash — never after a clean exit, so `stop` stays
stopped until the next login and `restart` bounces it with a fresh read of
its config. `autostart off` drops the login item and leaves a running server
alone; `autostart off stop` takes both down. A login item runs a bare
`start`, so its options are the `[connection.<name>]` entry in config.toml
(`statement-timeout`, `memory-limit`, `workers`, `threads`, `init`), never
flags. The verbs move two independent facts, the way `brew services` and
`systemctl` do:

| You type             | Registered at login | Running now       |
| -------------------- | ------------------- | ----------------- |
| `autostart`          | yes                 | yes               |
| `autostart stop`     | yes                 | no                |
| `stop`               | unchanged           | no                |
| `restart`            | unchanged           | yes               |
| `autostart off`      | no                  | unchanged         |
| `autostart off stop` | no                  | no                |
| `detach`             | no                  | unchanged         |
| `detach stop`        | no                  | no, and forgotten |

Removing the login item never kills a server: `autostart off` and `detach`
take the registration away and leave whatever is running to `stop` — though
until logout the manager still restarts that server after a crash, since it
holds the job it loaded.

There is no registry. The socket **is** the runtime registration: its name is
derived from the database's canonical path
(`~/.local/state/harbor/runtime/<basename>-<hash>.sock`), so discovery is
`readdir` plus a `GET /info` to each socket — which is precisely what bare
`harbor` prints:

```console
$ harbor
╭───────────────╮
│ harbor 0.32.1 │
├───────────────┴────┬───────────────────────┬─────────┬───────┬─────────┬────────╮
│ DATABASE           │ URL                   │ VERSION │ PID   │ CLIENTS │ UPTIME │
├────────────────────┼───────────────────────┼─────────┼───────┼─────────┼────────┤
│ ~/Data/labs.duckdb │ http://127.0.0.1:9495 │ 0.32.1  │ 72840 │       2 │     3d │ ¹
╰────────────────────┴───────────────────────┴─────────┴───────┴─────────┴────────╯
¹ ~/.local/state/harbor/runtime/labs.duckdb-1a2b3c4d.sock
```

The caption is the installed CLI version. Each row's version comes from that
running server, so an installation update is visible immediately and servers
that still need a restart stand out. The socket path hangs below the grid
under the row's footnote, and the URL column exists only while some server has
a TCP door — an all-socket fleet keeps the five-column shape.

A running database answers to four spellings, and the last two come straight
off this list:

```console
$ harbor ~/.local/state/harbor/runtime/labs.duckdb-1a2b3c4d.sock   # its socket
$ harbor http://127.0.0.1:9495                                     # its URL
$ harbor labs                                                      # its name
$ harbor 1                                                         # its footnote
```

A name or a footnote reaches what is listed — a running server by the name
it declares, an attached database by its config key — and a bare word never
becomes a file. A name two running databases share is refused as ambiguous
rather than guessed. The verbs take the same spellings: `harbor labs stop`,
`harbor 3 start`; opening a stopped one summons it the way its path would.

A socket nothing answers on is a leftover from a `kill -9`, and the list
unlinks it. Set `HARBOR_HOME` (absolute path) to collapse configuration and
runtime state — sockets, logs, and history — into one directory; the test
suites use it to keep their servers out of the real fleet view.

### Output modes

`--mode <m>` picks how results are rendered, and `.mode <m>` changes it at the
prompt. The display modes — `duckbox` (the default at a terminal), `duckboxy`
(the same without the type row), `markdown`, `line` and `list` — are for eyes.
The data modes — `csv`, `json` and `jsonlines` (`--json` is shorthand) — are
for programs, and boxed output on a pipe gets a hint to pick one. `trash`
discards results and reports only errors.

The modes differ in how they treat a `VARIANT` or `JSON` cell, which the wire
carries as JSON text. A display mode shows a `VARIANT` string bare — a value
read by path, `raw_request.requisitionNumber`, shows as `L2605106156`, the way
a `VARCHAR` always has — while a number, a boolean, a null, an object or an
array shows as its JSON; a `JSON` column shows its text with the quotes, as
DuckDB's own table does. Cast a path to `JSON` to see the quotes in a table.
`csv` carries the JSON text CSV-escaped, so `42` and `"42"` stay apart for the
program on the other end. `json` and `jsonlines` splice the cell in as the JSON
it is — `43`, not `"43"`; a document, not a string holding one — for both
column types; a SQL `NULL` and a JSON null are both `null` there. The text is
checked before it is spliced, so a record is always well-formed: `NaN` and
`Infinity`, which the engine writes bare and JSON cannot say, stay the strings
`"NaN"` and `"Infinity"`. The check does not recurse and has no depth limit, so
a document is spliced whole however deep it nests, and a consumer whose parser
stops at some depth meets that in its own parser.
A pretty-printed `JSON` column keeps its newlines in the engine; `jsonlines` is
one record per line, so between tokens they become spaces. JSON nested inside a
struct, list or map column is a string, as the wire holds it. The wire itself
is untouched by any mode.

### Backup and restore

A `.duckdb` file is only as portable as the engine that wrote it, so copying
one is a snapshot, not a backup. `backup` writes the durable thing instead —
`schema.sql`, `load.sql`, one tab-separated file per table, and `after.sql`
when a table has a `VARIANT` column (below):

```console
$ harbor medlabs backup
harbor: backed up 14 tables to ~/db/medlabs.backups/20260909044118 (612K)
harbor: restore it with — harbor <new.duckdb> restore ~/db/medlabs.backups/20260909044118

$ harbor fresh.duckdb restore ~/db/medlabs.backups/20260909044118 --block-size 64k
harbor: restored 14 tables into fresh.duckdb (740K)
```

Tab-separated rather than parquet, deliberately: both round-trip exactly and
both come to about the same size, so the tie goes to what you can do with the
artifact six months from now — grep it, diff two of them, read one in an
editor, keep one in a repo. Sequences come back at their current value and
indexes come back with them.

Three values and one escape cover the whole dialect: a bare `NULL` is a real
null, a quoted `"NULL"` is the string, and an empty field is an empty string.
Quotes are always *allowed* and rarely *required* — a value containing a tab,
a newline or a quote is wrapped and a `"` inside doubles to `""` (RFC 4180,
which every CSV reader already knows), and everything else is written plain.
Nothing is backslash-escaped, so a backslash in the data is only ever a
backslash. The reader takes a bare field and a written `""` the same way, so a
backup stays editable by hand.

One file per backup may look different, and it is a row of data rather than a
matter of taste. A one-column table holding an empty string would write an
empty LINE, and every CSV reader skips those — that row would not come back
and nothing would say so. `FORCE_QUOTE` is the only lever DuckDB offers and it
takes a column list rather than a predicate, so it is spent per file: a table
whose export contains a blank record is written again with every value quoted,
said out loud, and no other file pays for it. One thing worth knowing before
you reach for `wc -l`: a value holding a newline spans physical lines, so a
row is a record, not always a line.

**A `VARIANT` column travels as JSON.** Its display rendering cannot come
back — the number 42 and the string `"42"` both print as `42`, and the reader
hands every cell back as a string — but JSON tells them apart, and a value
that entered as JSON returns exactly as it entered, every inner type and
every nesting. So the column is written as JSON text (`42` for the number,
`"42"` for the string, still greppable) and decoded on restore. For a stock
`duckdb` the decode is an `UPDATE`, and DuckDB's `IMPORT DATABASE` takes
nothing but `COPY`, so it lives in `after.sql` beside `load.sql`: importing
the directory by hand gives the JSON text, and running the second file turns
it back into documents. `harbor restore` needs no second pass. It runs
`schema.sql` and each `COPY` of `load.sql` itself, which is all `IMPORT
DATABASE` does, and loads a table with `VARIANT` columns as documents: the
table's own `COPY`, options and all, fills a staging table that holds those
columns as text, one `INSERT` moves the rows across with the text cast through
JSON, and the staging table is dropped. The table never holds the JSON text,
so a `CHECK` on a `VARIANT` column is shown documents and nothing else. What
JSON has no word for — a `DATE`, a `DECIMAL`, a
`BLOB`, a `TIMESTAMP` put inside a variant from SQL — comes back as JSON's
nearest type, and the backup says so, once per column:

```console
$ harbor mydata.duckdb backup
harbor: events."payload" holds DATE — written as JSON, which has no such type; --format parquet keeps it
harbor: backed up 14 tables to ~/db/mydata.backups/20260909051315 (612K)
```

An integer stored from SQL as a narrow type comes back as JSON's wide one —
the same value, a different width label — which is what "written as JSON"
means and is not reported. `--strict` refuses instead of writing the note.

**A `GENERATED` column is computed, not carried.** `schema.sql` holds its
expression and no data file holds its values, in either format, since no
`COPY` takes one back. The restored table computes it from the rows it is
given, a column that reads a `VARIANT` document (`doc['a']::VARCHAR`) from
the restored document. A text file that does hold the generated columns says
so in its header, and restores the same way with those fields left behind.

**Neither format holds every shape**, and that is why `--format` exists:

| | tsv | parquet |
| --- | --- | --- |
| `UNION` | loses its tag — the restore refuses | ✅ |
| `VARIANT` nested in a `STRUCT`, `LIST` or `MAP` | would come back retyped — refused | no writer for it — refused |
| `VARIANT` holding a `DATE`, `DECIMAL`, `BLOB`, … | JSON's nearest type, *said out loud* | ✅ |
| `VARIANT` holding an `INTERVAL`, `BIGNUM`, `BIT`, … | JSON's nearest type, *said out loud* | refused outright |
| negative `INTERVAL` | ✅ | refused outright |
| `TIMETZ` with an offset | ✅ | normalised to UTC, *silently* |

A table the chosen format cannot carry is written in the other format when
that format can preserve all its types, and the change is reported:

```console
$ harbor mydata.duckdb backup
harbor: settings is parquet, not text — a UNION loses its tag
harbor: backed up 14 tables to ~/db/mydata.backups/20260909051315 (612K)
```

`load.sql` names the format per table, so the directory stays self-describing
and the choice is visible in `ls`. `--format parquet` asks for one format
throughout — with the same swap running the other way for a `TIMETZ` column,
whose table is then text in every respect, a `VARIANT` beside it included —
and `--strict` refuses rather than swapping, for a backup that has to be one
format or nothing. What no mode will do is write something that will not come
back without saying so: a negative interval under `--format parquet` is an
error. A table combining UNION with TIMETZ is refused because neither
whole-table format preserves it. So is a table holding a `VARIANT[]`, a
`STRUCT(v VARIANT)` or a `MAP(VARCHAR, VARIANT)`, in either format: text
reaches only a plain `VARIANT` column and parquet has no writer for one below
the root. The whole backup stops there and no directory is written —

```console
$ harbor mydata.duckdb backup
harbor: settings cannot round-trip in either backup format: parquet has no writer for a VARIANT nested inside another type
```

— and under `--format parquet` the refusal is the engine's own `Not
implemented Error`. Keep a `VARIANT` a plain column, or hold the nested shape
inside one `VARIANT` document, and the table backs up.

The whole of this is a test suite rather than a claim: `test/scripts/roundtrip.py`
backs up and restores every type in the shared corpus, a schema of constraints
and indexes and views and sequences, the strings that attack the format, and a
seeded fuzz of random tables — then attaches both databases and asks DuckDB
whether anything differs.

Backup holds one transaction for the initial export and every rewrite pass,
so concurrent committed table changes cannot mix snapshots. It needs a free
session connection. The CLI renews a 60-second lease every 20 seconds during
both SQL and file inspection, so there is no fixed total backup lifetime.
If the client disappears, the lease expires 60 seconds after its last successful
renewal (or creation if never renewed). Harbor cancels active work, then rolls
back and reclaims the connection once execution stops. A renewal failure or
failed pass aborts the backup and removes its incomplete directory; it never
resumes on a newer snapshot. The operator's `--statement-timeout` still limits each SQL statement;
configure it to accommodate the longest export pass. The server must support
renewable backup sessions; older servers produce an explicit upgrade error. Exported data is scanned with a bounded buffer. Sequence counters
are not transactional in DuckDB; quiesce sequence users when their exact
position must correspond to the exported rows.

The directory is self-contained. Each `COPY` in `load.sql` names its file and
nothing more, so the backup can be moved, renamed, copied to another machine
or committed to a repo and still restore — an absolute path would have nailed
it to the machine that wrote it.

`restore` always builds a **new** file and refuses one that exists. A restore
that can overwrite is a restore that can be run at the wrong moment and take
the very thing it was meant to protect; moving the restored file into place is
a human's job, and a deliberate one. It is also the only moment `--block-size`
can be applied, since DuckDB fixes that when a file is created and offers no
`ALTER` — which makes `backup` then `restore` the way to change it.

### Sockets and TCP

The Unix socket is always there, protected by the `0700` runtime directory.
`--port` adds loopback TCP *beside* the socket, never in place of it, so the
server stays visible to the fleet (the list, DuckTable, join-before-summon)
like any other. Both doors are machine-local:

```console
$ harbor mydata.duckdb start --port 9495
$ harbor http://127.0.0.1:9495 -c "SELECT count(*) FROM orders"
```

An explicit start can also take its port from the matching
`[connection.<name>]` entry in `~/.config/harbor/config.toml`; a summon stays
on the Unix socket, so opening a database never silently adds a TCP listener.
TCP binds IPv4 loopback only: `127.0.0.1`.

Remote access is Caddy's job at the edge (TLS and access policy); harbor itself speaks
plain HTTP over a unix socket or a loopback TCP port. A human reaches a
remote host over ssh and uses the socket.

Ordinary DuckDB SQL can read host files or load extensions. For a server whose
callers should not receive those capabilities, `--sealed` disables host-file
access and community extensions.
After startup initialization, Harbor locks its memory, thread and spill settings
inside DuckDB. SQL wrappers cannot override them or unlock configuration.
Other settings registered at startup remain changeable unless `--init` imposed
a stricter lock. Load extensions that need configurable settings during
initialization; settings registered later are outside the allowed list.

`--max-temp-size` bounds disk spill, and `--statement-timeout` places the
hard statement ceiling described above. These are independent of Caddy's
transport and HTTP policy.

### Request logging

`--log` writes one line per HTTP request to stderr:

```
harbor: 2026-08-12T04:31:07Z 127.0.0.1 POST /sql 200 12ms
```

Timestamp, peer, method, path, status, duration — measured to the last body
byte rather than the first, so a slow query and a slow client both show. Off by
default. The SQL itself is never logged: it arrives in the request body, it can
be megabytes, and on this endpoint it is as likely to hold customer data as the
tables it reads.

stderr, not stdout, so it stays clear of anything a client reads. Send it
wherever the log belongs — `2>>/var/log/harbor.log`, a pipe, or a supervisor's
collector. There is no `--log FILE`: rotation and permissions are the shell's
job, and it does them better than harbor would.

## Any language

There is nothing to install on the client side. Shell:

```console
$ curl -sN 127.0.0.1:9495/sql -H 'Content-Type: application/json' \
       -d '{"sql":"SELECT count(*) FROM orders"}'
```

Python, standard library only — NDJSON means one message per line, so the
response reads as it arrives:

```python
import http.client, json

conn = http.client.HTTPConnection("127.0.0.1", 9495)
conn.request("POST", "/sql", json.dumps({"sql": "SELECT id, total FROM orders"}),
             {"Content-Type": "application/json"})

for line in conn.getresponse():
    msg = json.loads(line)
    if msg["type"] == "row":
        print(msg["values"])
```

JavaScript, with `fetch` — and `params`, which is how values are passed:

```js
const res = await fetch("http://127.0.0.1:9495/sql", {
  method: "POST",
  headers: { "Content-Type": "application/json" },
  body: JSON.stringify({ sql: "SELECT id, total FROM orders WHERE id > ?",
                         params: [100] }),
});

const decoder = new TextDecoder();
let pending = "";
for await (const chunk of res.body) {
  pending += decoder.decode(chunk, { stream: true });
  const lines = pending.split("\n");
  pending = lines.pop();
  for (const line of lines) {
    if (!line.trim()) continue;
    const msg = JSON.parse(line);
    if (msg.type === "row") console.log(msg.values);
  }
}
pending += decoder.decode();
if (pending.trim()) {
  const msg = JSON.parse(pending);
  if (msg.type === "row") console.log(msg.values);
}
```

## Performance

DuckDB answers the query; DuckDB Harbor's job is to stay out of the way. It
sustains tens of thousands of requests per second across concurrent clients
on a laptop, with sub-100µs round trips at low concurrency.

**harbor 0.13.0, DuckDB v2.0.0 nightly** (alpha38195), eight workers, pure
read path — `POST /sql` with `{"sql":"select 1"}` over keep-alive loopback
TCP, 10-second `oha` runs, every response a 200:

| clients | req/s | p50 | p99 |
|--:|--:|--:|--:|
| 1 | 10,914 | 0.09 ms | 0.12 ms |
| 4 | 28,167 | 0.14 ms | 0.22 ms |
| 16 | 44,079 | 0.24 ms | 0.61 ms |

The HTTP layer is not the ceiling: `GET /ready` — the same plumbing with no
SQL — measures ~99,000 req/s at 16 clients. Most of the per-request engine
cost is reduced by the per-connection parsed-statement cache (below);
0.13.0 also coalesced each response head into a single buffered write, set
`TCP_NODELAY`, and removed most per-request allocations from the HTTP layer.

An earlier, deliberately harsher benchmark — 20% `INSERT`s, every read
checked against an oracle, harbor 0.12.0 (no statement cache), **DuckDB
v1.5.5**, eight workers:

| clients | req/s | p50 | p95 | p99 | non-200 | wrong answers |
|--:|--:|--:|--:|--:|--:|--:|
| 1 | 3,269 | 0.20 ms | 0.58 ms | 0.74 ms | 0 | 0 |
| 4 | 7,012 | 0.50 ms | 1.18 ms | 1.40 ms | 0 | 0 |
| 16 | 9,096 | 1.66 ms | 2.90 ms | 3.60 ms | 0 | 0 |

Mean of five 10-second runs per level on an idle M-series laptop, connections
reused, throughput taken from wall-clock across the level rather than summed
from per-request timings. Run-to-run spread was under 4% at every level.

The engine version belongs beside the numbers, because it moves them. The same
harbor build on a **v2.0.0** nightly gets roughly half this on small statements
— 1,352 / 3,667 / 4,739 req/s at the same three levels (alpha37626; still true
of alpha38195). That is not a debug build and it is not harbor. It is v2's [new
PEG parser](https://duckdb.org/2026/08/20/duckdb-20-peg-parser), plus a small
fixed cost per execute — measured by driving each engine directly, no server:
re-executing an already-prepared statement costs +11 µs on v2, while parsing
fresh SQL text costs about 2× v1.5.5, growing with statement size. Execution
itself is at parity or faster (bulk CTAS is quicker on v2 than on 1.5.5).
The current v2 implementation caches parsed SQL, not bound execution plans.
Each execution still binds against the current catalog. Each connection keeps
at most 64 texts and 1 MiB of SQL text, skips caching individual statements
larger than 64 KiB, and clears its cache when the connection is replaced. AST
allocations are additional engine memory; these limits bound retained SQL text,
not total process RSS. Historical benchmark figures above describe their stated
versions; measure the current build against the engine you deploy.

Every read in the mixed run was checked against an answer taken from the database
file before the server opened it — a benchmark whose oracle is the server it is
benchmarking cannot detect a server that is consistently wrong.

Streaming matters more than the rate for large results. A 300,000-row result
starts arriving in single-digit milliseconds — before the query has finished
running — and completes in well under 100 ms, because nothing is buffered. A
client can start work on row one while the server is still producing row
300,000. (Whether the *query* materialises is DuckDB's business: `ORDER BY`,
hash aggregates and joins all build state first.)

Many connections, few queries: DuckDB Harbor accepts many concurrent
connections and executes a small, bounded number of statements — six by
default, settable with `--workers`. DuckDB parallelises a *single* query across
every core, so running hundreds at once produces thrashing, not throughput. A
request normally waits for a worker. If every worker has been inside a
statement for at least 250 ms, the dedicated probe lane keeps control routes
responsive and may shed new `/sql` or `/catalog` work with a retryable `503`
instead of hiding an unbounded queue behind saturated analytics.

## Why it looks like this

**Plain HTTP, on purpose.** It binds loopback and speaks HTTP, not HTTPS. TLS
belongs at the edge, where certificates, renewal, and HTTP/2 and /3 are already
solved by software that does nothing else. Put Caddy or nginx in front and
terminate there.

**One statement per request.** A second statement is rejected with `400`, and
that check is load-bearing rather than decorative: the Rust DuckDB client
*executes* every statement but the last while merely preparing one, so anything
that gets past it runs. Use `params` for values.

**Types survive the trip.** Every column carries its `duckdbType`, plus width
and scale for `DECIMAL` and nested `child`/`fields` for `LIST` and `STRUCT`, so
a typed client can reconstruct exactly what DuckDB had rather than a lossy JSON
approximation. Values JSON cannot hold exactly are quoted rather than emitted
as bare numbers, so an integer past 2^53 does not silently reprecision in a
JavaScript client. Where something genuinely cannot survive, the schema says so
with `"lossless": false` instead of returning a plausible wrong answer. The
flag sits on the type that loses: for a `VARIANT[]`, a `STRUCT(v VARIANT)` or a
`MAP(VARCHAR, VARIANT)` that is the `child`, the field or the `valueType`, and
the column's own entry still reads `"lossless": true`, so a client that wants
the answer for a nested column reads the nested entries too.

## Where it fits

DuckDB's ecosystem already covers DuckDB talking to DuckDB. DuckDB Harbor
covers everyone else.

| Serves | Client needs |
| --- | --- |
| `quack` — other DuckDB instances | DuckDB |
| **`harbor` — everything else** | **`curl`** |

`quack` is a DuckDB extension; `harbor` is a standalone server. It can
still load an extension into its own database with
`harbor db.duckdb start --unsigned --init 'LOAD <ext>'`, so one process can
answer HTTP clients and other DuckDB instances over one file at once. Harbor
ships no extension of its own — whatever `LOAD` resolves by name in `~/.duckdb`
is what it gets, matching that to the loaded engine is the operator's call, and
Harbor does not patch extension source while loading it. For a desktop face on
Harbor servers, [DuckTable](../ducktable/) is the
native client, developed in this repository beside harbor.

## Known limitations

**`VARIANT` arrives as JSON text, `GEOMETRY` as display text.** Neither has a
committed vector layout in the v2 C API, so each cell crosses as one value. A
`VARIANT` is cast to JSON on the way out, so `42` and `'42'` stay apart and a
document that entered as JSON leaves as the same JSON; the column says
`"lossless": false, "encoding": "json"` because JSON has no `DATE` or
`TIMESTAMP` of its own (those arrive as strings). A `GEOMETRY` goes out as the
engine's own text rendering under `"encoding": "varchar-cast"`. (Two
limitations this section used to carry are gone: `TIME WITH TIME ZONE` keeps
its offset since 0.22, and `TIME_NS` encodes since 0.21.)

**A `VARIANT` is JSON at every edge, and not quite JSON inside.** A column
that always enters as JSON can be stored as `VARIANT` for the engine's typed
field access, and still be handled as JSON by everything outside: harbor
delivers it as JSON text, and `harbor backup` writes it as JSON text. The
trip JSON → `VARIANT` → JSON is exact for real documents — any string, any
Unicode, key order, `1` versus `1.0`, `"42"` versus `42`, integers up to 64
bits, nested nulls — with these known edges, all measured on the engine:

- *Text written into a `VARIANT` needs `::JSON`.* A plain string written
  into a `VARIANT` column, whether a literal, an `UPDATE`, or a string
  parameter, is stored as a string, not parsed: `'{"a":1}'` lands as the text
  `{"a":1}`, a client gets `"{\"a\":1}"` back, and every path into it is NULL.
  Write `'{"a":1}'::JSON`, or `$1::JSON` for a parameter, and it lands as an
  object. `'…'::VARIANT` does not parse either. The same applies to a column
  conversion: use
  `ALTER TABLE t ALTER COLUMN c SET DATA TYPE VARIANT USING c::JSON::VARIANT`.
  harbor's object and array parameters, Rip's ORM and DuckTable's editor all
  write a document as a document, so the mistake is one hand-written SQL makes.
- *A `CHECK` on a `VARIANT` column restores.*
  `CHECK (v IS NULL OR variant_typeof(v) LIKE 'OBJECT%')` does refuse the
  unparsed string at write time, and with it every array, every scalar and
  every JSON string, which are documents too, so it is not a general guard.
  `harbor backup` writes such a table as text like any other and `harbor
  restore` brings it back, SQL `NULL` included, because it loads documents
  as documents and the constraint is never shown the JSON text they
  travelled as. A stock `duckdb` cannot restore that table from the same
  directory: `IMPORT DATABASE` lands each cell as a `VARIANT` string for
  `after.sql` to decode, and the constraint refuses the strings first, with
  `Constraint Error: CHECK constraint failed`. `--format parquet` carries the
  column as itself, and stock `duckdb` imports that directory.
- *An object or array parameter is a document.* `"params": [{"a":1}]` aimed at
  a `VARIANT` — a column in `SET` or `VALUES`, a comparison against one, a
  `coalesce` with one — is bound as the document, so `SET doc = ?` stores an
  object and `WHERE doc = ?` finds one. harbor asks the engine what each
  parameter expects, once, and only for a request that carries an object or
  an array. Aimed anywhere else it is its JSON text, a VARCHAR, for the
  statement to cast: an untyped slot (`SELECT ?`, `INSERT … SELECT ?`), a
  `VARCHAR` column, a `$1` used against two types, a `VARIANT[]` or a
  `STRUCT` holding one. A string parameter is a string wherever it goes,
  whatever it spells: a parameter that looks like JSON is data, as one that
  looks like SQL is. A client holding JSON *text* — a grid cell, a file —
  says so with `?::JSON`. An object or array parameter nests at most 100
  levels, its own levels counted: `{}` is one and `[[1]]` is two. One level
  more is a 400 `bad_request` that says `a document param nests at most 100
  levels`, and nothing is bound. Rip's ORM and DuckTable's editor keep the
  same number, so a document is refused at the same depth whichever layer
  meets it first. A string parameter is never inspected: what it spells is
  data, and one cast through `?::JSON` nests as deep as the engine reads.
- *Integers beyond 64 bits become doubles* and lose digits past the 17th;
  numbers past the range of a double come back as `Infinity`, which is not
  JSON. Integers within `INT64`/`UINT64` are exact.
- *A top-level JSON `null` is SQL `NULL`* once stored; nested nulls survive.
- *Duplicate keys keep the last value*, and once a table is large enough for
  the engine to shred the column into typed paths, *keys may come back
  sorted*. The document is the same; its text is not. Anything that hashes
  or signs the body text must keep the original text.
- *Normalization*, the same as DuckDB's `json()`: whitespace and `\u`
  escapes are dropped, `-0` is `0`, `1.50` is `1.5`, `1e10` is
  `10000000000.0`. An object or array parameter is read by harbor's request
  parser first, and two numbers differ there: `-0` is stored as the `DOUBLE`
  `-0.0`, where the `?::JSON` text path stores the integer `0`, and `1e400`
  fails the request with a 400, `number out of range`, where the text path
  stores `Infinity`.
- *Fields are typed.* `v.status = 500` finds rows; `v.status = '500'` finds
  none, without an error. `v.tests.price` through an array is `NULL`, not
  an error.
- *`v::VARCHAR` is display text, not JSON*, so `v->'a'`, `json_*(v)`, `LIKE`
  on the column and `COPY … TO 'x.csv'` all see `{'a': 1}`; use `v.a`,
  `v['a']`, or `v::JSON->'a'`, and cast to `::JSON` before a text export.
- *Delivery casts one cell at a time.* The engine's C API has no batch cast,
  so harbor delivers a `VARIANT` column at roughly 20,000 rows a second
  against 400,000 for the same documents as `VARCHAR`, measured on 1.5 KB
  documents. `SELECT v::JSON` in the query is vectorized and delivers at
  130,000. Fetching one document, or a hundred, does not notice.

Need more fidelity than that? Back the table up with `--format parquet`,
which keeps the `VARIANT` as itself.

**Bodies are capped at 8 MiB**, declared or delivered; over that is a `413`.
There is no rate limiting and no CORS. **A web page cannot reach the TCP
listener**: a request carrying an `Origin` header, or a `Host` that is a hostname
rather than `localhost` or an IP address, is refused with `403 forbidden` before
anything runs. That shuts out a page on the same machine posting SQL to
loopback, and DNS rebinding reading the answer. Harbor's own clients, Rip and
curl send no `Origin`, and their `Host` is whatever address they were given:
`http://127.0.0.1:9495` or `http://localhost:9495` passes, including through
an SSH tunnel, but a name that only `/etc/hosts` maps to loopback does not. A browser client belongs behind an edge proxy that enforces
its own policy, drops `Origin`, and sends the upstream's address as `Host`
(Caddy: `header_up -Origin` and `header_up Host {upstream_hostport}`). The unix
socket is out of a browser's reach and skips the check. Request logging is available with
`--log`, off by default.

**Windows serves over loopback TCP only.** Unix sockets — and with them
spawn-on-use and the list — are a unix feature. On Windows, serving is
explicit (`harbor <db> start --port <p>`) and the client half
works the same everywhere.

**The engine is the loaded `libduckdb`, not the binary.** Nothing is linked:
harbor loads the engine on demand (`HARBOR_LIBDUCKDB`, then `../lib` beside
the binary, `~/.local/lib`, and `~/.duckdb/cli/*` — DuckDB's own world,
disposable and refetchable). Harbor binds DuckDB's v2 C API, so DuckDB 2.0
is the engine floor; the same build has been verified against every
v2-API engine it has met, and CI runs the suite against DuckDB's current
nightly of the 2.0 branch. Treat that as tested compatibility, not a
promise that an arbitrary future DuckDB ABI will work. Your database files
need no such care: a file created by a 1.5-era DuckDB opens as-is, because
2.0's storage layer reads it. A machine with
no engine at all still runs the client half; only serving needs the library,
and the error says exactly where it looked.

## Working on it

Building is only needed to change it. The workspace has four first-party
crates:

- **`harbor`** — the server engine, the client (`src/repl/`, which never
  touches DuckDB), and the CLI, all in one binary;
- **`harbor-common`** — paths, names, permissions, durations: the vocabulary
  shared with DuckTable so the two cannot drift;
- **`wire`** — protocol request and response types consumed by the client
  half; and
- **`justhttp`** — Harbor's small synchronous HTTP/1.1 server over TCP and
  Unix sockets.

The server implements its protocol shapes directly rather than depending on
`wire`, so a wire change needs tests on both sides; drift is not a Rust
compile error. Nothing links `libduckdb` — the engine loads on demand — so no
DuckDB source tree, library, or header is required to build: `make harbor`
works on a bare machine, and `make fetch-duckdb` fetches the duckdb CLI plus
a library (honoring `DUCKDB_LIB_BUILD`, under "Get it running"). The
crate ships pregenerated bindings, so there is no bindgen.

`make unit` runs the fast Rust tests and `make test` runs the full suite. The
full suite expects `sample.duckdb`; create it with
`test/scripts/fixture.sh sample.duckdb` when it is absent. CI performs that
fixture step explicitly. The thirteen suites use independent oracles where answers
need comparison — values read from the database file before the server takes
the lock, and Python's own `datetime` and `base64` for fuzzed values. An oracle
that shares an implementation with the thing it checks confirms only that the
code is self-consistent.

## Status

Pre-production. One small binary. Nothing is linked: harbor loads a DuckDB
2.0+ `libduckdb` at runtime — the v2 C API is the floor — and database files
from 1.5-era DuckDBs open as-is. Deploy remote TCP behind Caddy, which owns
TLS and edge request
policy; Harbor independently owns SQL statement deadlines.

## License

MIT.
