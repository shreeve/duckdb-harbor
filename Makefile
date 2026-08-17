# harbor — the fleet Makefile
#
# One binary, dynamically linked (D1/Phase 0): harbor links an external
# libduckdb and is version-agnostic at runtime — it resolves whichever
# libduckdb.dylib sits beside it (@loader_path), so the same bytes serve 1.5.5
# or 2.0 depending on the directory they run from. `make install` proves it by
# dropping one harbor + pilot into every ~/.duckdb/cli/<ver>/. pilot never links
# an engine and works against all of them.
#
# A self-contained static build (crates.io `bundled` DuckDB 1.5.5) is still
# available on demand — `cargo build -p harbor --features bundled` — but is no
# longer built or shipped: it is a 33MB artifact (and a 17GB from-source C++
# build tree) whose one advantage, needing no sibling dylib, the dylib-swap
# model removed.

.PHONY: all binary pilot check check_quick install fetch-duckdb clean

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
# library in place. `make fetch-duckdb UI=1` instead pulls a pinned, matched
# engine+UI pair from our releases (the ui extension only loads against the exact
# engine it was built with). CI links the same official nightly via
# .github/actions/duckdb. See PLAN.md D11.
fetch-duckdb:
	DEST=$(DUCKDB_LIB) UI=$(UI) scripts/fetch-duckdb.sh

clean:
	cargo clean
