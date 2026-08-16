.PHONY: clean clean_all check check_quick

PROJ_DIR := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))

EXTENSION_NAME=harbor

# Set to 1 to enable Unstable API (binaries will only work on TARGET_DUCKDB_VERSION, forwards compatibility will be broken)
# Note: currently extension-template-rs requires this, as duckdb-rs relies on unstable C API functionality
USE_UNSTABLE_C_API=1

# Target DuckDB version
TARGET_DUCKDB_VERSION=v1.5.5

all: configure debug

# Include makefiles from DuckDB
include extension-ci-tools/makefiles/c_api_extensions/base.Makefile
include extension-ci-tools/makefiles/c_api_extensions/rust.Makefile

configure: venv platform extension_version

debug: build_extension_library_debug build_extension_with_metadata_debug
release: build_extension_library_release build_extension_with_metadata_release

test: test_debug
test_debug: test_extension_debug
test_release: test_extension_release

# Build for every DuckDB this tree is expected to serve, and park each one where
# bin/duckdb-harbor looks for it.
#
# This exists because the alternative is a trap. `make release` leaves its
# artifact in build/release stamped for TARGET_DUCKDB_VERSION, and the launcher
# prefers build/release-<version-of-the-duckdb-you-are-running>. So a tree built
# for v2 has a build/release the abi and sqllogictest suites reject, and a tree
# built for v1.5.5 runs the live suites against whatever stale copy happens to
# be in the stamped directory. Both failures look like bugs in harbor.
#
# Add a version here when the tree starts targeting one.
STAMPED_DUCKDB_VERSIONS ?= v2.0.0-alpha37626

# Never `cp` onto an extension that is already there. cp truncates and rewrites
# in place, so the file keeps its inode -- and macOS has already cached a
# code-signature verdict for that inode from the previous binary. The next
# dlopen finds contents that do not match the cached signature and the kernel
# SIGKILLs the process: `duckdb -c "LOAD ..."` dies with exit 137, no message,
# no log line, while a byte-identical copy at any other path loads fine. Writing
# beside it and renaming gives a fresh inode and no cached verdict.
define stamp_to
	mkdir -p build/release-$(1)
	cp build/release/harbor.duckdb_extension build/release-$(1)/.harbor.next
	mv -f build/release-$(1)/.harbor.next build/release-$(1)/harbor.duckdb_extension
endef

.PHONY: release_all
release_all: release
	@for v in $(STAMPED_DUCKDB_VERSIONS); do \
	  echo "==> $$v"; \
	  $(MAKE) --no-print-directory release TARGET_DUCKDB_VERSION=$$v || exit 1; \
	  mkdir -p build/release-$$v; \
	  cp build/release/harbor.duckdb_extension build/release-$$v/.harbor.next; \
	  mv -f build/release-$$v/.harbor.next build/release-$$v/harbor.duckdb_extension; \
	done
	@echo "==> $(TARGET_DUCKDB_VERSION) (left in build/release)"
	@$(MAKE) --no-print-directory release TARGET_DUCKDB_VERSION=$(TARGET_DUCKDB_VERSION)
	@$(call stamp_to,$(TARGET_DUCKDB_VERSION))

# check runs every suite: unit tests, sqllogictest, and the HTTP suites that
# need a live server. See test/scripts/check.sh for the ordering and for SUITES=.
check: release_all
	@test/scripts/check.sh

# The subset that runs in under a minute, for the edit/build/test loop.
check_quick: release
	@SUITES="unit sqllogic spec fuzz" test/scripts/check.sh

clean: clean_build clean_rust
clean_all: clean_configure clean

# ==[ Fleet binaries (PLAN.md Phase 1) ]==
# The bundled channel embeds DuckDB 1.5.5 from crates.io; the 2.0 channel
# links the prebuilt libduckdb from the duckdb checkout. Separate cargo
# invocations on purpose: bundled and loadable-extension cannot feature-unify.
DUCKDB2_LIB ?= $(HOME)/Data/Code/duckdb/build/release/src
DUCKDB2_INC ?= $(HOME)/Data/Code/duckdb/src/include

.PHONY: binary binary2 fleet-check
binary:
	cargo build -p harbor-bin -p harbor-pilot --release

binary2:
	DUCKDB_LIB_DIR=$(DUCKDB2_LIB) DUCKDB_INCLUDE_DIR=$(DUCKDB2_INC) \
	cargo build -p harbor-bin --no-default-features --release --target-dir target-2
	@install_name_tool -add_rpath $(DUCKDB2_LIB) target-2/release/harbor 2>/dev/null || true

fleet-check: binary binary2
	cd test/scripts && for s in sessions cancel catalog; do \
	  echo "==[ $$s (binary, 2.0) ]=="; \
	  HARBOR_LAUNCHER="$(PROJ_DIR)target-2/release/harbor serve" python3 $$s.py || exit 1; \
	done
