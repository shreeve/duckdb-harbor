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
