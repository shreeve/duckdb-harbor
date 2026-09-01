# harbor — the Makefile
#
# One binary, nothing linked: `harbor` loads libduckdb on demand (dlopen),
# so the build needs no engine at all and a pure client runs on machines
# without one. The engine lives in ~/.duckdb (DuckDB's world), version-scoped
# so several can coexist; the binary installs into ~/.local/bin. `make
# fetch-duckdb` pulls the engine (DuckDB's official v2 nightly) into
# ~/.duckdb; `make install` copies the binary to $(BIN). No step needs root.

.PHONY: all harbor unit test install fetch-duckdb bootstrap clean

# The libduckdb `make fetch-duckdb` installs, and one of the places harbor
# looks at runtime (see crates/harbor/src/engine.rs for the full search
# order; HARBOR_LIBDUCKDB overrides it). Only the library is needed — the
# crate ships pregenerated bindings, so there is no bindgen and no header
# requirement.
DUCKDB_LIB ?= $(HOME)/.duckdb/cli/2.0.0

# Where the binary lands — outside DuckDB's ~/.duckdb world, which is
# disposable and refetchable. ~/.local/bin is the XDG home for user
# executables, needs no root, and on Debian and Fedora is already on PATH.
# Override to install elsewhere (BIN=/usr/local/bin needs sudo in front).
BIN ?= $(HOME)/.local/bin

all: harbor

harbor:
	cargo build -p harbor --release

unit:
	cargo test --release

test: harbor
	test/scripts/check.sh

# Copy the binary onto PATH in $(BIN).
# The rm first is load-bearing on macOS: overwriting a binary in place leaves
# the kernel's per-inode signature cache stale, and every exec dies by SIGKILL
# ("valid on disk", still killed). A fresh inode gets a fresh verdict.
# The stale `pilot` is swept on upgrade: it merged into harbor in 0.20.
install: harbor
	@install -d -m 0755 "$(BIN)"
	@rm -f "$(BIN)/harbor" "$(BIN)/pilot"
	@install -m 0755 target/release/harbor "$(BIN)/harbor"
	@echo "installed harbor -> $(BIN)"
	@case ":$$PATH:" in *":$(BIN):"*) ;; \
	  *) echo "note: $(BIN) is not on your PATH — add it to your shell rc" ;; esac

# Pull DuckDB's official v2.0-dev nightly (libduckdb + headers + duckdb CLI)
# from artifacts.duckdb.org into $(DUCKDB_LIB), where harbor's runtime search
# finds it. CI fetches the same official nightly via .github/actions/duckdb.
fetch-duckdb:
	DEST=$(DUCKDB_LIB) scripts/fetch-duckdb.sh

# From scratch: fetch the engine into ~/.duckdb, and build + install harbor
# onto PATH ($(BIN)). One command from an empty ~/.duckdb to a working v2
# setup, with no step that needs root.
bootstrap: fetch-duckdb install
	@echo "bootstrap: engine in ~/.duckdb, harbor in $(BIN) — on $(notdir $(DUCKDB_LIB))"

clean:
	cargo clean
