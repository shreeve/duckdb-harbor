#!/usr/bin/env bash
#
# install.sh — install this harbor release from the extracted archive.
#
#   bin/harbor, bin/pilot -> /usr/local/bin   (override: BIN=...)
#   lib/libduckdb.*       -> /usr/local/lib   (override: LIB=...)
#   ui.duckdb_extension   -> ~/.duckdb/extensions/<version>/<platform>/
#
# The binaries carry a relative rpath (../lib), so bin + lib travel as a pair —
# they also run straight out of this directory without installing. The ui
# extension goes where `LOAD ui` resolves it by name; ENGINE (written at
# package time) names the exact <version>/<platform> it was built for.
# sudo is used only if the system dirs are root-owned.

set -euo pipefail
cd "$(dirname "$0")"

BIN=${BIN:-/usr/local/bin}
LIB=${LIB:-/usr/local/lib}
read -r version ext_plat < ENGINE

as_owner() { if [ -w "$(dirname "$1")" ] || [ -w "$1" ]; then "${@:2}"; else sudo "${@:2}"; fi; }
as_owner "$BIN" install -d -m 0755 "$BIN"
as_owner "$LIB" install -d -m 0755 "$LIB"
as_owner "$BIN" install -m 0755 bin/harbor bin/pilot "$BIN"
as_owner "$LIB" install -m 0755 lib/libduckdb.* "$LIB"

ext_dir="$HOME/.duckdb/extensions/$version/$ext_plat"
mkdir -p "$ext_dir"
install -m 0644 extensions/ui.duckdb_extension "$ext_dir/ui.duckdb_extension"

echo "installed: harbor + pilot -> $BIN, libduckdb -> $LIB"
echo "           ui extension   -> $ext_dir  (engine $version)"
echo "try: harbor add mydata.duckdb --unsigned --init 'LOAD ui; FROM start_ui_server();'"
