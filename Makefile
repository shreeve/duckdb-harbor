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

# check runs every suite: unit tests, sqllogictest, and the HTTP suites that
# need a live server. See scripts/check.sh for the ordering and for SUITES=.
check: release
	@scripts/check.sh

# The subset that runs in under a minute, for the edit/build/test loop.
check_quick: release
	@SUITES="unit sqllogic spec fuzz" scripts/check.sh

clean: clean_build clean_rust
clean_all: clean_configure clean
