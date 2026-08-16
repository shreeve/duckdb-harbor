# DuckDB Harbor Manual

## Install DuckDB 1.5.5

```zsh
curl https://install.duckdb.org | sh
```

```zsh
# install DuckDB Quack
duckdb -c "INSTALL quack"
```

```zsh
# install DuckDB UI
duckdb -c "INSTALL ui"
```

```zsh
# install DuckDB Harbor
rm -rf ~/tmp-duckdb
mcd ~/tmp-duckdb
  curl -sLO  \
    https://github.com/shreeve/duckdb-harbor/releases/download/v0.8.1/harbor-v0.8.1-duckdb-v1.5.5-linux_amd64.zip
  unzip -o "${_:t}"
  duckdb -unsigned -c "FORCE INSTALL './harbor.duckdb_extension'; LOAD harbor; FROM harbor_version();"
rm -rf ~/tmp-duckdb
cd
```

```zsh
# launch DuckDB Harbor
curl -sL -o ~/bin/duckdb-harbor \
  https://github.com/shreeve/duckdb-harbor/releases/download/v0.8.1/duckdb-harbor
chmod +x ~/bin/duckdb-harbor
rm -rf mydata.duckdb ; duckdb mydata.duckdb -c CHECKPOINT
duckdb-harbor mydata.duckdb --ui --quack --log --token rip-token
```

## Install DuckDB 2.0.0

```zsh
rm -rf ~/tmp-duckdb
mcd ~/tmp-duckdb
curl -sLO \
  https://github.com/shreeve/duckdb-harbor/releases/download/duckdb-v2.0.0-alpha37626/duckdb-v2.0.0-alpha37626-binaries-linux-amd64.zip
unzip -o duckdb-v2.0.0-alpha37626-binaries-linux-amd64.zip
unzip -o duckdb_cli-linux-amd64.zip
mcd ~/.duckdb/cli/2.0.0
mv ~/tmp-duckdb/duckdb .
ln -snf ~/.duckdb/cli/{2.0.0,latest}
rm -rf ~/tmp-duckdb
cd
```

```console
duckdb -c 'pragma version'
-- Loading resources from /home/shreeve/.duckdbrc
┌───────────────────┬────────────┬────────────┐
│  library_version  │ source_id  │  codename  │
│      varchar      │  varchar   │  varchar   │
├───────────────────┼────────────┼────────────┤
│ v2.0.0-alpha37626 │ 7e14bd24e0 │ Cyanoptera │
└───────────────────┴────────────┴────────────┘
```

```zsh
# install DuckDB Quack
duckdb -c "INSTALL quack"
```

```zsh
# install DuckDB UI (unsigned for 2.0.0)
rm -rf ~/tmp-duckdb
mcd ~/tmp-duckdb
  curl -sLO \
    https://github.com/shreeve/duckdb-ui/releases/download/ui-v2.0.0-alpha37626/ui-duckdb-v2.0.0-alpha37626-linux_amd64.zip
  unzip -o ui-duckdb-v2.0.0-alpha37626-linux_amd64.zip
  duckdb -unsigned -c "FORCE INSTALL './ui.duckdb_extension'"
rm -rf ~/tmp-duckdb
cd
```

```zsh
# install DuckDB Harbor
rm -rf ~/tmp-duckdb
mcd ~/tmp-duckdb
  curl -sLO  \
    https://github.com/shreeve/duckdb-harbor/releases/download/v0.8.1/harbor-v0.8.1-duckdb-v2.0.0-alpha37626-linux_amd64.zip
  unzip -o "${_:t}"
  duckdb -unsigned -c "FORCE INSTALL './harbor.duckdb_extension'; LOAD harbor; FROM harbor_version();"
rm -rf ~/tmp-duckdb
cd
```

```zsh
# launch DuckDB Harbor
curl -sL -o ~/bin/duckdb-harbor \
  https://github.com/shreeve/duckdb-harbor/releases/download/v0.8.1/duckdb-harbor
chmod +x ~/bin/duckdb-harbor
rm -rf mydata.duckdb ; duckdb mydata.duckdb -c CHECKPOINT
duckdb-harbor mydata.duckdb --ui --quack --log --token rip-token
```

## Switch versions

```zsh
ln -snf ~/.duckdb/cli/{2.0.0,latest}
ln -snf ~/.duckdb/cli/{1.5.5,latest}
```

# macOS version

```


# DuckDB Harbor v0.8.2
https://github.com/shreeve/duckdb-harbor/releases/download/v0.8.2/duckdb-harbor




DUCKDB_VER="2.0.0"
DUCKDB_TAG="alpha37626"
DUCKDB_SYS="linux-arm64"
DUCKDB_SYS="linux-amd64"
DUCKDB_SYS="osx" # adds "-universal" to some
DUCKDB_HBV="0.8.2"

DUCKDB_ALL="v${DUCKDB_VER}-${DUCKDB_TAG}"
DUCKDB_BIN="https://github.com/shreeve/duckdb-harbor/releases/download/duckdb-${DUCKDB_ALL}/duckdb-${DUCKDB_ALL}-binaries-${DUCKDB_SYS}.zip"
DUCKDB_UIX="https://github.com/shreeve/duckdb-ui/releases/download/ui-${DUCKDB_ALL}/ui-duckdb-${DUCKDB_ALL}-linux_amd64.zip"
DUCKDB_UIX="https://github.com/shreeve/duckdb-ui/releases/download/ui-v2.0.0-alpha37626/ui-duckdb-v2.0.0-alpha37626-osx_arm64.zip"
DUCKDB_HBX="https://github.com/shreeve/duckdb-harbor/releases/download/v0.8.1/harbor-v0.8.1-duckdb-v2.0.0-alpha37626-linux_amd64.zip"
DUCKDB_HBX="https://github.com/shreeve/duckdb-harbor/releases/download/v0.8.1/harbor-v0.8.1-duckdb-v2.0.0-alpha37626-osx_arm64.zip"
DUCKDB_HBL="https://github.com/shreeve/duckdb-harbor/releases/download/v0.8.1/duckdb-harbor"

# ==[ Install DuckDB CLI ]==

item=(
  https://github.com/duckdb/duckdb/releases/download/v1.5.5/duckdb_cli-linux-amd64.zip
  https://github.com/duckdb/duckdb/releases/download/v1.5.5/duckdb_cli-linux-arm64.zip
  https://github.com/duckdb/duckdb/releases/download/v1.5.5/duckdb_cli-osx-universal.zip
  https://github.com/duckdb/duckdb/releases/download/v1.5.5/duckdb_cli-windows-amd64.zip
  https://github.com/duckdb/duckdb/releases/download/v1.5.5/duckdb_cli-windows-arm64.zip
)

item=(
  https://github.com/shreeve/duckdb-harbor/releases/download/duckdb-v2.0.0-alpha37626/duckdb-v2.0.0-alpha37626-binaries-linux-amd64.zip
  https://github.com/shreeve/duckdb-harbor/releases/download/duckdb-v2.0.0-alpha37626/duckdb-v2.0.0-alpha37626-binaries-linux-arm64.zip
  https://github.com/shreeve/duckdb-harbor/releases/download/duckdb-v2.0.0-alpha37626/duckdb-v2.0.0-alpha37626-binaries-osx.zip
  https://github.com/shreeve/duckdb-harbor/releases/download/duckdb-v2.0.0-alpha37626/duckdb-v2.0.0-alpha37626-binaries-windows.zip
)

URL=$item[3]
VER=${${URL#*duckdb-v}%%-*}

rm -rf ~/tmp-duckdb
mcd ~/tmp-duckdb
  curl -sLO "${URL}"
  unzip -oq "${URL:t}"
  unzip -oq duckdb_cli-*.zip
  unzip -oq libduckdb-*.zip
  mcd ~/.duckdb/cli/$VER
  mv ~/tmp-duckdb/{duckdb,libduckdb.*,*.h} .
  ln -snf ~/.duckdb/cli/{$VER,latest}
rm -rf ~/tmp-duckdb
cd

duckdb -c 'PRAGMA version'

# ==[ Install DuckDB Quack ]==

duckdb -c "INSTALL quack"

# ==[ Install DuckDB UI ]==

# DuckDB UI for v2.0.0-alpha37626
item=(
  https://github.com/shreeve/duckdb-ui/releases/download/ui-v2.0.0-alpha37626/ui-duckdb-v2.0.0-alpha37626-linux_amd64.zip
  https://github.com/shreeve/duckdb-ui/releases/download/ui-v2.0.0-alpha37626/ui-duckdb-v2.0.0-alpha37626-linux_arm64.zip
  https://github.com/shreeve/duckdb-ui/releases/download/ui-v2.0.0-alpha37626/ui-duckdb-v2.0.0-alpha37626-osx_arm64.zip
  https://github.com/shreeve/duckdb-ui/releases/download/ui-v2.0.0-alpha37626/ui-duckdb-v2.0.0-alpha37626-windows_amd64.zip
  https://github.com/shreeve/duckdb-ui/releases/download/ui-v2.0.0-alpha37626/ui-duckdb-v2.0.0-alpha37626-windows_arm64.zip
)

URL=$item[3]

rm -rf ~/tmp-duckdb
mcd ~/tmp-duckdb
  curl -sLO "${URL}"
  unzip -oq "${URL:t}"
  duckdb -unsigned -c "FORCE INSTALL './ui.duckdb_extension'"
rm -rf ~/tmp-duckdb
cd

==

item=(
  https://github.com/shreeve/duckdb-harbor/releases/download/v0.9.0/harbor-v0.9.0-duckdb-v1.5.5-linux_amd64.zip
  https://github.com/shreeve/duckdb-harbor/releases/download/v0.9.0/harbor-v0.9.0-duckdb-v1.5.5-linux_arm64.zip
  https://github.com/shreeve/duckdb-harbor/releases/download/v0.9.0/harbor-v0.9.0-duckdb-v1.5.5-osx_arm64.zip
  https://github.com/shreeve/duckdb-harbor/releases/download/v0.9.0/harbor-v0.9.0-duckdb-v1.5.5-windows_amd64.zip
  https://github.com/shreeve/duckdb-harbor/releases/download/v0.9.0/harbor-v0.9.0-duckdb-v1.5.5-windows_arm64.zip
)

# ==[ Install DuckDB Harbor ]==

item=(
  https://github.com/shreeve/duckdb-harbor/releases/download/v0.9.1/harbor-v0.9.1-duckdb-v2.0.0-alpha37626-linux_amd64.zip
  https://github.com/shreeve/duckdb-harbor/releases/download/v0.9.1/harbor-v0.9.1-duckdb-v2.0.0-alpha37626-linux_arm64.zip
  https://github.com/shreeve/duckdb-harbor/releases/download/v0.9.1/harbor-v0.9.1-duckdb-v2.0.0-alpha37626-osx_arm64.zip
  https://github.com/shreeve/duckdb-harbor/releases/download/v0.9.1/harbor-v0.9.1-duckdb-v2.0.0-alpha37626-windows_amd64.zip
  https://github.com/shreeve/duckdb-harbor/releases/download/v0.9.1/harbor-v0.9.1-duckdb-v2.0.0-alpha37626-windows_arm64.zip
)

URL=$item[3]

rm -rf ~/tmp-duckdb
mcd ~/tmp-duckdb
  curl -sLO "${URL}"
  unzip -oq "${URL:t}"
  duckdb -unsigned -c "FORCE INSTALL './harbor.duckdb_extension'; LOAD harbor; FROM harbor_version();"
rm -rf ~/tmp-duckdb
cd

URL="https://github.com/shreeve/duckdb-harbor/releases/download/v0.8.2/duckdb-harbor"

curl -sL -o ~/bin/duckdb-harbor "${URL}"
chmod +x ~/bin/duckdb-harbor
rm -rf mydata.duckdb ; duckdb mydata.duckdb -c CHECKPOINT
duckdb-harbor mydata.duckdb --ui --quack --log --token rip-token

# ==

$ auth="Authorization: Bearer rip-token"
$ curl -s localhost:9495/catalog -H "$auth"

{"harborVersion":"0.9.0","duckdbVersion":"v2.0.0-alpha37626","tables":[],"sequences":[]}









# What's actually configured
The medlabs container has an incus proxy device named duckui:

duckui:
  bind: host
  connect: tcp:[::1]:9497
  listen: unix:/opt/janus/run/duckui.sock
  uid: "997"   gid: "986"   mode: "0660"
  type: proxy

That's the socket-side bridge for exactly the container problem I described. bind: host puts the listening end on the host, so incus creates a real unix socket at /opt/janus/run/duckui.sock — inside the directory install provisions — and forwards every connection over to tcp:[::1]:9497 inside the container. The app in the container keeps speaking plain TCP on loopback and needs to know nothing about Janus; the host side of the pipe is a unix socket, which is the only thing Janus will dial.

The uid/gid are the part that makes it work, and they're correct. On the host, uid 997 and gid 986 are precisely the janus service account and group. Combined with mode: 0660, the socket comes out srw-rw---- janus janus, so the Janus process can both read and write it. Get those numbers wrong and the socket exists but every request dies at dial time with a permission error — the same late, invisible failure mode as registering a TCP address.

It's also worth noting the two sockets in that directory are doing opposite jobs: janus.sock is Janus's own control API (s-w-------, inbound, apps register through it), while duckui.sock is an upstream Janus dials outbound. They coexist in run/ because install sets that directory to janus:janus 0750 and doesn't touch its contents recursively, so re-running ./install won't clobber the proxy device's socket.

==

Here's the exact one-time command I ran — the "lease":"process" field is the whole point: it registers duckui with no heartbeats required, and Janus keeps it until an explicit DELETE or a restart.

Run on live (where the control plane is reachable), via the loopback port:

incus config device add medlabs duckui proxy \
  bind=host \
  listen=unix:/opt/janus/run/duckui.sock \
  connect=tcp:[::1]:9497 \
  uid=997 gid=986 mode=0660

duckui:
  type: proxy
  bind: host
  listen: unix:/opt/janus/run/duckui.sock   # ← host side: incus creates THIS socket
  connect: tcp:[::1]:9497                    # ← container side: forwards to DuckDB's UI port
  uid: "997"    # janus
  gid: "986"    # janus
  mode: "0660"

# The uid=997 gid=986 mode=0660 is the load-bearing part: 997/986 are the janus user/group on the host, so the socket comes out srw-rw---- janus janus and the Janus process can read/write it. Get those numbers wrong and the socket exists but Janus can't open it.

curl -sS -X POST http://127.0.0.1:7600/1.0/apps \
  -H 'content-type: application/json' \
  -d '{
        "name": "duckui",
        "hosts": ["duckdb.medlabs.health"],
        "upstreams": [{"path": "/opt/janus/run/duckui.sock"}],
        "lease": "process"
      }'
It returns {"id":"duckui-xxxxxx"} on success (HTTP 201).

Equivalent form over the unix control socket (no port needed, but requires sudo for socket permissions — this is the "operator on the box" path):


sudo curl -sS -X POST --unix-socket /opt/janus/run/janus.sock http://janus/1.0/apps \
  -H 'content-type: application/json' \
  -d '{"name":"duckui","hosts":["duckdb.medlabs.health"],"upstreams":[{"path":"/opt/janus/run/duckui.sock"}],"lease":"process"}'
A few things worth knowing:

"lease":"process" vs the default. Omit lease and it defaults to "heartbeat", which gets reaped after ~15s unless something keeps POSTing heartbeats. "process" is the no-heartbeat kind — it just sits there until removed. That's why a single curl is enough.
It does not survive a Janus restart. The registry is memory-only, so this exact command is what you re-run after any restart (which is what I did after deploying v1.6.6).
To remove it: DELETE /1.0/apps/duckui-xxxxxx using the id from the create response (or GET /1.0/apps to look it up).
The socket path in upstreams is the host side of the incus proxy device — it must already exist (it does, at /opt/janus/run/duckui.sock), but the registration and the socket are independent: registering only tells Janus where to route.

```

## GET /catalog

One authenticated call, the complete schema — tables, columns, primary keys,
unique constraints, indexes, foreign keys, sequences — as one JSON document
whose shape never varies with the DuckDB version harbor links. Foreign keys
and unique constraints come from the engine's structured catalog fields,
never from parsing constraint text, and the output is byte-stable for a
stable database: tables ordered by (schema, name), columns in ordinal
position, indexes and sequences by name, unique constraints by their column
lists. `401` without the bearer token, exactly like `GET /sessions`.

```console
$ curl -s localhost:9495/catalog -H "Authorization: Bearer rip-token" | jq
{
  "harborVersion": "0.8.2",
  "duckdbVersion": "v2.0.0-alpha37626",
  "tables": [
    {
      "name": "posts",
      "schema": "main",
      "columns": [
        { "name": "id",      "type": "INTEGER", "notNull": true,  "default": null, "primary": true  },
        { "name": "user_id", "type": "INTEGER", "notNull": false, "default": null, "primary": false },
        { "name": "title",   "type": "VARCHAR", "notNull": false, "default": null, "primary": false }
      ],
      "primaryKey": ["id"],
      "uniqueConstraints": [],
      "indexes": [
        { "name": "idx_posts_title", "columns": ["title"], "unique": false }
      ],
      "foreignKeys": [
        { "columns": ["user_id"], "refTable": "users", "refSchema": "main", "refColumns": ["id"] }
      ]
    },
    {
      "name": "users",
      "schema": "main",
      "columns": [
        { "name": "id",    "type": "INTEGER", "notNull": true,  "default": "nextval('users_seq')", "primary": true  },
        { "name": "email", "type": "VARCHAR", "notNull": true,  "default": null, "primary": false },
        { "name": "name",  "type": "VARCHAR", "notNull": false, "default": null, "primary": false }
      ],
      "primaryKey": ["id"],
      "uniqueConstraints": [
        { "columns": ["email"] }
      ],
      "indexes": [],
      "foreignKeys": []
    }
  ],
  "sequences": [
    { "name": "users_seq", "start": 1 }
  ]
}
```

`type` is the canonical DuckDB type string as the catalog reports it, and
`default` is the default expression text (or null) — both belong to the
engine. `primary` is true only for primary-key member columns; `primaryKey`
is the ordered column list, an empty array when there is none.
`uniqueConstraints` holds every UNIQUE constraint on the table — an inline
`UNIQUE` on a column and a table-level `UNIQUE (a, b)` alike — each entry's
`columns` in declaration order, the list sorted by its column lists, an
empty array when there are none; PRIMARY KEY is never in it. `indexes`
holds only the indexes `CREATE INDEX` made — the internal ART indexes that
implement PRIMARY KEY and UNIQUE column constraints are not indexes here,
matching `duckdb_indexes()`; the users example above is the distinction in
one glance: its uniqueness is a constraint, so `indexes` is empty.
`duckdbVersion` is read from the running engine (`pragma_version()`), so
the same harbor build reports whichever DuckDB it is actually serving.
