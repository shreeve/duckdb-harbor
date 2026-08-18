# harbor — the fleet Makefile
#
# Two binaries, dynamically linked (D1): `harbor` links an external libduckdb by
# an absolute rpath (baked at build time), `pilot` links no engine at all. The
# engine lives in ~/.duckdb (DuckDB's world: dylib + extensions); the two
# binaries install onto PATH in /usr/local/bin. `make fetch-duckdb` pulls the
# engine (DuckDB's official v2 nightly) into ~/.duckdb; `make install` copies
# both binaries to $(BIN) (using sudo only if the dir is root-owned).

.PHONY: all binary pilot check check_quick install fetch-duckdb ui setup clean

# The libduckdb the build links against, by absolute rpath — the engine
# `make fetch-duckdb` installs. Only the library is needed (the crate ships
# pregenerated bindings, so there is no bindgen and no header requirement).
# Override to link a different one.
DUCKDB_LIB ?= $(HOME)/.duckdb/cli/2.0.0

# Where the two binaries land — a stable dir on PATH, outside DuckDB's ~/.duckdb
# world (which is disposable/refetchable). Override to install elsewhere.
BIN ?= /usr/local/bin

# Every cargo invocation below links against that libduckdb and bakes an rpath
# to it, so the binary AND the test executables run in place without DYLD_*.
export DUCKDB_LIB_DIR := $(DUCKDB_LIB)
export RUSTFLAGS      := -C link-args=-Wl,-rpath,$(DUCKDB_LIB)

all: binary pilot

binary:
	cargo build -p harbor --release

pilot:
	cargo build -p pilot --release

check: binary pilot
	test/scripts/check.sh

check_quick: binary pilot
	SUITES="unit types spec catalog sessions cancel" test/scripts/check.sh

# Copy the two binaries onto PATH in $(BIN). harbor keeps its baked absolute
# rpath to $(DUCKDB_LIB), so it finds libduckdb from anywhere — no @loader_path,
# no sibling dylib. pilot links no engine and runs on any machine. sudo is used
# only when $(BIN) is root-owned (the /usr/local/bin default on macOS).
# The rm first is load-bearing on macOS: overwriting a binary in place leaves
# the kernel's per-inode signature cache stale, and every exec dies by SIGKILL
# ("valid on disk", still killed). A fresh inode gets a fresh verdict.
install: binary pilot
	@sudo= ; [ -w "$(BIN)" ] || { sudo=sudo; echo "  $(BIN) is root-owned — using sudo"; }; \
	  $$sudo install -d -m 0755 "$(BIN)"; \
	  $$sudo rm -f "$(BIN)/harbor" "$(BIN)/pilot"; \
	  $$sudo install -m 0755 target/release/harbor "$(BIN)/harbor"; \
	  $$sudo install -m 0755 target/release/pilot  "$(BIN)/pilot"; \
	  echo "installed harbor + pilot -> $(BIN)  (on your PATH)"

# Pull DuckDB's official v2.0-dev nightly (libduckdb + headers + duckdb CLI) from
# artifacts.duckdb.org into $(DUCKDB_LIB); the baked rpath then resolves the
# library in place. CI links the same official nightly via .github/actions/duckdb.
# The matched UI extension is built against this by `make ui`. See PLAN.md D11.
fetch-duckdb:
	DEST=$(DUCKDB_LIB) scripts/fetch-duckdb.sh

# Build the DuckDB UI extension against the exact nightly now in $(DUCKDB_LIB)
# and install it where `LOAD ui` finds it by name. Everything derives from that
# one engine, so the UI, the dylib, and harbor are synchronized by construction.
# See scripts/build-ui-extension.sh (and PLAN.md D11).
ui:
	DUCKDB_LIB=$(DUCKDB_LIB) scripts/build-ui-extension.sh

# Reconstruct a working fleet from scratch: fetch the engine into ~/.duckdb,
# build + install harbor and pilot onto PATH ($(BIN)), and build the matched UI
# extension. One command from an empty ~/.duckdb to a working v2 fleet with the UI.
setup: fetch-duckdb binary pilot install ui
	@echo "setup: engine + UI in ~/.duckdb, harbor + pilot in $(BIN) — all on $(notdir $(DUCKDB_LIB))"

clean:
	cargo clean
