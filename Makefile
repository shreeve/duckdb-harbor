# harbor — the fleet Makefile (the extension machinery retired with PLAN.md D5)
#
# Two engine channels, one codebase (D1/Phase 0):
#   binary   — embeds DuckDB 1.5.5 via crates.io `bundled`  → target/release/harbor
#   binary2  — links the prebuilt 2.0-dev libduckdb          → target-2/release/harbor
# pilot never links an engine and works against both.

.PHONY: all binary binary2 pilot check check_quick clean

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

clean:
	cargo clean
	rm -rf target-2
