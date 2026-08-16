# DuckDB UI v2 compatibility delta

Harbor on DuckDB 2.0.0 does not use the official `duckdb/duckdb-ui` build.
It uses [shreeve/duckdb-ui](https://github.com/shreeve/duckdb-ui) `main`,
which is a fork of upstream `main` plus the changes below, built against
DuckDB nightly `v2.0.0-alpha37626` (`7e14bd24e0`).

The hosted frontend is unchanged. The app at `https://ui.duckdb.org` is a
fixed bundle: it is not versioned against the DuckDB you are running, and
it only understands the 1.5.x wire format (field ids `100`–`106`, `200`–`201`).
DuckDB 2.0.0 changed what the server puts on that wire. The extension
compiles clean against v2 and then the app fails at runtime.

This file is the delta: what vanilla UI does, what our fork does instead,
and why Harbor needs it.

**Range:** `60497eb` … `48159d7` on `shreeve/duckdb-ui` (12 commits).
Upstream `duckdb/duckdb-ui` `main` is still the 1.5.x line
(`244552d`).

## Required for the UI to work on v2

Without these three, the official extension against DuckDB 2.0.0 is
unusable. None of them is a build error.

### 1. Pin result serialization to 1.5.5

Vanilla v2 `Vector::Serialize` writes VARCHAR as fields `107` / `108` /
`109`. The app's `readVector` requires `102`. That kills the first query
the app runs, so the UI never initializes.

The fork pins the one `BinarySerializer::Serialize` that emits `/ddb/run`
results to `StorageVersion::V1_5_5`. Unpin when the client learns the
new form.

### 2. Report `SQLNULL` columns as `INTEGER`

Vanilla 1.5.x resolved a bare `NULL` literal to `INTEGER`. Vanilla 2.0.0
leaves the column typed `SQLNULL` (type id `1`). The app's
`LogicalTypeId` starts at `BOOLEAN: 10` and has no entry for it:

```
Error syncing remote state: unrecognized type id: 1
```

The app emits `NULL as <name>` for columns it has no values for, so this
fires on startup and after every cell run.

A `SQLNULL` vector is physically `INT32` and serializes byte-for-byte
like an all-null `INTEGER`. The fork rewrites `SQLNULL` → `INTEGER` in
the declared types only, recursing through `LIST`, `ARRAY`, `MAP`,
`STRUCT`, and `UNION`. Subtrees with no `SQLNULL` are left alone.
Unions use `UnionType::GetMemberCount`, not `StructType::GetChildCount`
(the latter also returns the hidden tag member).

### 3. Drop the phantom tokenize token

Vanilla 2.0.0 `Parser::Tokenize` appends one extra token at
`content.size()` — one past the last character. 1.5.x did not.

The app splits a cell on these offsets, closing a statement at each `;`.
The phantom reopens a statement after the semicolon already ended one, so
one Run sends two `/ddb/run` requests. Visible as:

```
Catalog Error: Table with name "foo" already exists!
```

The fork drops tokens that do not point at a character. A comment-only
cell still emits a real `COMMENT` token at a real offset; that is left
alone.

## Required to compile and start on v2

### 4. DuckDB v2 API shims

Vanilla UI does not compile against current DuckDB `main`. The fork
keeps the 1.5.x build byte-identical and version-guards four API changes:

- table-function bind `out_names` is `vector<Identifier>`, not
  `vector<string>`
- `BaseQueryResult::names` / `::types` are private (`GetNames` /
  `GetTypes`)
- catalog names are `Identifier` (`AsCatalogIdentifier` /
  `AsRawString`)
- a `DataChunk`'s cardinality comes from child-vector sizes;
  `SetCardinality(1)` + `SetValue` leaves a malformed one-row chunk.
  `AppendSingleValue` uses `Vector::Append` on v2

### 5. Watcher waits before the first poll

Vanilla polls immediately, then sleeps. The first catalog check races
the still-running query that started the thread. On v2 that showed up as
intermittent crashes and `column out of range` during `start_ui_server`.
The fork waits one polling interval (~284 ms) first. Same change on
1.5.x; the cost is one delayed refresh event.

## Required for Harbor's reverse-proxy setup

### 6. `ui_public_url` names the origin the server will accept

Vanilla data endpoints (`/ddb/run`, `/ddb/interrupt`, `/ddb/tokenize`,
and `/localToken` via `Referer`) accept only
`Origin: http://localhost:<ui_local_port>`. That is a real cross-site
guard. It also means a UI reached through a reverse proxy loads its page
and then 401s every query, because the browser reports the proxy's name.

The fork adds `ui_public_url` (env `UI_PUBLIC_URL`). When set, that
string is the one allowed origin. When empty, behaviour is the old
`local_url` check. The guard still admits exactly one origin; it is
relocated, not removed. Rewriting `Origin` in the proxy would make the
comparison unfailable and delete the protection.

`local_url` is unchanged: it is still what the server calls itself in
logs and in the “UI is at …” message.

## Not required at runtime

These are in the same commit range. They do not change what the app
sees.

| Change | Why it is in the fork |
| --- | --- |
| Pin `duckdb` + `extension-ci-tools` to `alpha37626` | local and CI builds target the same nightly Harbor uses |
| `.github/workflows/V2Build.yml` + Makefile `v2` target | produce unsigned `ui` extensions for all five native platforms |
| `DUCKDB_UI_LOG_REQUESTS=1` | print each `/ddb/run` the hosted app sent; off by default |
| `docs/DUCKDB_V2.md` | diagnosis notes in the UI repo |

## What we did not change

- The hosted app at `ui.duckdb.org`. There is nothing newer to deploy;
  MotherDuck staging has the same field-id set.
- Upstream `duckdb/duckdb-ui`. This fork is Harbor's UI, not a PR
  series against official `main`.
- Harbor's own HTTP surface. Harbor talks NDJSON/JSON on `:9495`. The
  UI extension is a separate process/port that the official frontend
  speaks to.

## Check

Same query against stock 1.5.5 UI and this fork, compare response bytes.
Anything other than identical is a difference the app may not survive.
The current fork is byte-identical to 1.5.5 across the type space
(strings, integers, temporal, nested, `NULL` at every depth).
