# harbor — the fleet Makefile (the extension machinery retired with PLAN.md D5)
#
# Two engine channels, one codebase (D1/Phase 0):
#   binary   — embeds DuckDB 1.5.5 via crates.io `bundled`  → target/release/harbor
#   binary2  — links the prebuilt 2.0-dev libduckdb          → target-2/release/harbor
# pilot never links an engine and works against both.

.PHONY: all binary binary2 pilot check check_quick install clean

# Where the 2.0-dev DuckDB build lives (override for CI)
DUCKDB2_LIB     ?= $(HOME)/Data/Code/duckdb/build/release/src
DUCKDB2_INCLUDE ?= $(HOME)/Data/Code/duckdb/src/include

all: binary pilot

binary:
	cargo build -p harbor --release

binary2:
	DUCKDB_LIB_DIR=$(DUCKDB2_LIB) DUCKDB_INCLUDE_DIR=$(DUCKDB2_INCLUDE) \
	  cargo build -p harbor --no-default-features --release --target-dir target-2
	install_name_tool -add_rpath $(DUCKDB2_LIB) target-2/release/harbor 2>/dev/null || true

pilot:
	cargo build -p harbor-pilot --release

# The suites create fixtures with the local `duckdb` CLI; when that CLI is a
# 2.0 build (see MANUAL.md), the berths must link 2.0 too — so check prefers
# the binary2 channel when it exists and falls back to bundled.
check: binary pilot
	@if [ -x target-2/release/harbor ]; then \
	  HARBOR_LAUNCHER="$(CURDIR)/target-2/release/harbor serve" test/scripts/check.sh; \
	else \
	  test/scripts/check.sh; \
	fi

check_quick: binary pilot
	@if [ -x target-2/release/harbor ]; then \
	  HARBOR_LAUNCHER="$(CURDIR)/target-2/release/harbor serve" SUITES="unit types spec catalog sessions cancel" test/scripts/check.sh; \
	else \
	  SUITES="unit types spec catalog sessions cancel" test/scripts/check.sh; \
	fi

# One binary, swap dylib (proven): the dynamic harbor links libduckdb by its
# @rpath name and resolves whichever libduckdb.dylib sits beside it via
# @loader_path — so ONE binary drives either engine, chosen by the directory it
# runs from (verified: same bytes report v1.5.5 next to the 1.5.5 dylib and
# v1.6.0-dev next to the 2.0 dylib). `install` drops that binary, plus the
# engine-agnostic pilot, into every ~/.duckdb/cli/<ver>/ that has a dylib. The
# dev-tree absolute rpath is stripped from the installed copies so only the
# sibling dylib resolves (otherwise it would prefer the build-dir 2.0 dylib in
# every dir). Re-runnable; each copy is fresh, so the strip stays idempotent.
DUCKDB_CLI ?= $(HOME)/.duckdb/cli
install: binary2 pilot
	@for d in $(DUCKDB_CLI)/*/; do \
	  case "$$d" in *latest/) continue;; esac; \
	  [ -f "$${d}libduckdb.dylib" ] || continue; \
	  cp target-2/release/harbor "$${d}harbor"; \
	  cp target/release/pilot "$${d}pilot"; \
	  install_name_tool -delete_rpath $(DUCKDB2_LIB) "$${d}harbor" 2>/dev/null || true; \
	  install_name_tool -add_rpath @loader_path "$${d}harbor" 2>/dev/null || true; \
	  echo "  installed harbor + pilot -> $$d"; \
	done
	@echo "each cli/<ver>/harbor now uses its sibling libduckdb.dylib; pilot is engine-agnostic."

clean:
	cargo clean
	rm -rf target-2
