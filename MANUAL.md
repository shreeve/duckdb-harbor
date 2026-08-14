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

DUCKDB_VER="2.0.0"
DUCKDB_TAG="alpha37626"
DUCKDB_SYS="linux-arm64"
DUCKDB_SYS="linux-amd64"
DUCKDB_SYS="osx" # adds "-universal" to some

DUCKDB_ALL="v${DUCKDB_VER}-${DUCKDB_TAG}"
DUCKDB_BIN="https://github.com/shreeve/duckdb-harbor/releases/download/duckdb-${DUCKDB_ALL}/duckdb-${DUCKDB_ALL}-binaries-${DUCKDB_SYS}.zip"
DUCKDB_UIX="https://github.com/shreeve/duckdb-ui/releases/download/ui-${DUCKDB_ALL}/ui-duckdb-${DUCKDB_ALL}-linux_amd64.zip"
DUCKDB_UIX="https://github.com/shreeve/duckdb-ui/releases/download/ui-v2.0.0-alpha37626/ui-duckdb-v2.0.0-alpha37626-osx_arm64.zip"
DUCKDB_HBX="https://github.com/shreeve/duckdb-harbor/releases/download/v0.8.1/harbor-v0.8.1-duckdb-v2.0.0-alpha37626-linux_amd64.zip"
DUCKDB_HBX="https://github.com/shreeve/duckdb-harbor/releases/download/v0.8.1/harbor-v0.8.1-duckdb-v2.0.0-alpha37626-osx_arm64.zip"
DUCKDB_HBL="https://github.com/shreeve/duckdb-harbor/releases/download/v0.8.1/duckdb-harbor"

rm -rf ~/tmp-duckdb
mcd ~/tmp-duckdb
  curl -sLO "${DUCKDB_BIN}"
  unzip -oq "${DUCKDB_BIN:t}"
  unzip -oq duckdb_cli-*.zip
  unzip -oq libduckdb-*.zip
  mcd ~/.duckdb/cli/$DUCKDB_VER
  mv ~/tmp-duckdb/{duckdb,libduckdb.*,*.h} .
  ln -snf ~/.duckdb/cli/{$DUCKDB_VER,latest}
rm -rf ~/tmp-duckdb
cd

duckdb -c 'PRAGMA version'

duckdb -c "INSTALL quack"

rm -rf ~/tmp-duckdb
mcd ~/tmp-duckdb
  curl -sLO "${DUCKDB_UIX}"
  unzip -oq "${DUCKDB_UIX:t}"
  duckdb -unsigned -c "FORCE INSTALL './ui.duckdb_extension'"
rm -rf ~/tmp-duckdb
cd

rm -rf ~/tmp-duckdb
mcd ~/tmp-duckdb
  curl -sLO "${DUCKDB_HBX}"
  unzip -oq "${DUCKDB_HBX:t}"
  duckdb -unsigned -c "FORCE INSTALL './harbor.duckdb_extension'; LOAD harbor; FROM harbor_version();"
rm -rf ~/tmp-duckdb
cd

curl -sL -o ~/bin/duckdb-harbor "${DUCKDB_HBL}"
chmod +x ~/bin/duckdb-harbor
rm -rf mydata.duckdb ; duckdb mydata.duckdb -c CHECKPOINT
duckdb-harbor mydata.duckdb --ui --quack --log --token rip-token






```
