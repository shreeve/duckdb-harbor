# harbor — the fleet Makefile
#
# One binary, dynamically linked (D1/Phase 0): harbor links an external
# libduckdb and is version-agnostic at runtime — it resolves whichever
# libduckdb.dylib sits beside it (@loader_path), so the same bytes serve any
# DuckDB depending on the directory they run from. `make fetch-duckdb` pulls the
# engine (DuckDB's official v2 nightly); `make install` drops one harbor + pilot
# into every ~/.duckdb/cli/<ver>/. pilot never links an engine and works against
# all of them.

.PHONY: all binary pilot check check_quick install fetch-duckdb ui setup clean

# The libduckdb the dynamic build links against — the version installed under
# ~/.duckdb/cli/<ver>/. Only the library is needed (the crate ships pregenerated
# bindings, so there is no bindgen and no header requirement). The binary is
# version-agnostic at runtime, so this only picks what the dev build links;
# override to build against another, e.g.
#   make binary DUCKDB_LIB=$(HOME)/.duckdb/cli/1.5.5
DUCKDB_LIB ?= $(HOME)/.duckdb/cli/2.0.0
DUCKDB_CLI ?= $(HOME)/.duckdb/cli

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

# Drop one dynamic harbor + the engine-agnostic pilot into every
# ~/.duckdb/cli/<ver>/ that has a libduckdb.dylib. The dev-tree rpath is stripped
# from the installed copy and replaced with @loader_path, so each copy resolves
# ONLY its sibling dylib (otherwise it would prefer the build-dir dylib in every
# dir). Re-runnable; each copy is fresh, so the strip stays idempotent.
install: binary pilot
	@for d in $(DUCKDB_CLI)/*/; do \
	  case "$$d" in *latest/) continue;; esac; \
	  [ -f "$${d}libduckdb.dylib" ] || continue; \
	  cp target/release/harbor "$${d}harbor"; \
	  cp target/release/pilot "$${d}pilot"; \
	  install_name_tool -delete_rpath $(DUCKDB_LIB) "$${d}harbor" 2>/dev/null || true; \
	  install_name_tool -add_rpath @loader_path "$${d}harbor" 2>/dev/null || true; \
	  echo "  installed harbor + pilot -> $$d"; \
	done
	@echo "each cli/<ver>/harbor now uses its sibling libduckdb.dylib; pilot is engine-agnostic."

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

# Reconstruct ~/.duckdb from scratch: fetch the engine, build + install harbor
# and pilot beside it, and build the matched UI extension. One command to go
# from an empty ~/.duckdb to a working v2 fleet with the UI.
setup: fetch-duckdb binary pilot install ui
	@echo "setup: ~/.duckdb ready — harbor, pilot, and the UI all on $(notdir $(DUCKDB_LIB))"

clean:
	cargo clean
