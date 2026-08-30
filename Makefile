# harbor — the fleet Makefile
#
# Two binaries, dynamically linked: `harbor` links an external libduckdb by
# an absolute rpath (baked at build time), `pilot` links no engine at all. The
# engine lives in ~/.duckdb (DuckDB's world), version-scoped so several can
# coexist; the two binaries install into ~/.local/bin. `make fetch-duckdb`
# pulls the engine (DuckDB's official v2 nightly) into ~/.duckdb; `make
# install` copies both binaries to $(BIN). Nothing here needs root.

.PHONY: all harbor pilot unit test install fetch-duckdb bootstrap clean

# The libduckdb the build links against, by absolute rpath — the engine
# `make fetch-duckdb` installs. Only the library is needed (the crate ships
# pregenerated bindings, so there is no bindgen and no header requirement).
# Override to link a different one.
DUCKDB_LIB ?= $(HOME)/.duckdb/cli/2.0.0

# Where the two binaries land — outside DuckDB's ~/.duckdb world, which is
# disposable and refetchable. ~/.local/bin is the XDG home for user
# executables, needs no root, and on Debian and Fedora is already on PATH.
# Override to install elsewhere (BIN=/usr/local/bin needs sudo in front).
BIN ?= $(HOME)/.local/bin

# Every cargo invocation below links against that libduckdb and bakes an rpath
# to it, so the binary AND the test executables run in place without DYLD_*.
export DUCKDB_LIB_DIR := $(DUCKDB_LIB)
export RUSTFLAGS      := -C link-args=-Wl,-rpath,$(DUCKDB_LIB)

all: harbor pilot

harbor:
	cargo build -p harbor --release

pilot:
	cargo build -p pilot --release

unit:
	cargo test --release

test: harbor pilot
	test/scripts/check.sh

# Copy the two binaries onto PATH in $(BIN). harbor keeps its baked absolute
# rpath to $(DUCKDB_LIB), so it finds libduckdb from anywhere — no @loader_path,
# no sibling dylib. pilot links no engine and runs on any machine.
# The rm first is load-bearing on macOS: overwriting a binary in place leaves
# the kernel's per-inode signature cache stale, and every exec dies by SIGKILL
# ("valid on disk", still killed). A fresh inode gets a fresh verdict.
install: harbor pilot
	@install -d -m 0755 "$(BIN)"
	@rm -f "$(BIN)/harbor" "$(BIN)/pilot"
	@install -m 0755 target/release/harbor "$(BIN)/harbor"
	@install -m 0755 target/release/pilot  "$(BIN)/pilot"
	@echo "installed harbor + pilot -> $(BIN)"
	@case ":$$PATH:" in *":$(BIN):"*) ;; \
	  *) echo "note: $(BIN) is not on your PATH — add it to your shell rc" ;; esac

# Pull DuckDB's official v2.0-dev nightly (libduckdb + headers + duckdb CLI) from
# artifacts.duckdb.org into $(DUCKDB_LIB); the baked rpath then resolves the
# library in place. CI links the same official nightly via .github/actions/duckdb.
# Everything links against this one engine.
fetch-duckdb:
	DEST=$(DUCKDB_LIB) scripts/fetch-duckdb.sh

# Reconstruct a working fleet from scratch: fetch the engine into ~/.duckdb,
# and build + install harbor and pilot onto PATH ($(BIN)). One command from an
# empty ~/.duckdb to a working v2 fleet, with no step that needs root.
bootstrap: fetch-duckdb install
	@echo "bootstrap: engine in ~/.duckdb, harbor + pilot in $(BIN) — all on $(notdir $(DUCKDB_LIB))"

clean:
	cargo clean
