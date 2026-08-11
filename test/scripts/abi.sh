#!/usr/bin/env bash
#
# abi.sh — assert what the built artifact claims about itself, and what that
# claim costs.
#
#   scripts/abi.sh [path/to/harbor.duckdb_extension]
#   ALT_DUCKDB=/path/to/other/duckdb scripts/abi.sh
#
# Every other suite drives a running server. This one reads the binary. The
# 512-byte metadata footer is what DuckDB's loader reads before it will map the
# library at all, and nothing else here looks at it — so a build that stamps
# the wrong thing produces an extension that loads nowhere, and every suite
# still passes because they all run against a build that was stamped correctly
# by hand.
#
# The footer is written by extension-ci-tools/scripts/append_extension_metadata.py
# as eight 32-byte NUL-padded fields followed by 256 reserved bytes:
#
#   [ FIELD8 | FIELD7 | FIELD6 | FIELD5 | FIELD4 | FIELD3 | FIELD2 | FIELD1 ][ 256 ]
#     unused   unused   unused   abi     ext.ver  duckdb   platform  "4"
#                                type             version
#
# so counting back from the end of the file: FIELD1 at -288, FIELD2 at -320,
# FIELD3 at -352, FIELD4 at -384, FIELD5 at -416.
#
# The second half of the suite asserts the *consequence* of the stamp rather
# than the stamp alone, because the stamp is only interesting for what it does
# to loading. C_STRUCT_UNSTABLE pins the binary to one exact DuckDB version;
# C_STRUCT is forward-loadable. Whichever this build claims, loading it into a
# different engine has to behave accordingly — and it has to fail cleanly
# rather than hang or crash, which is the part a person hits at 2am.

set -uo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
ext=${1:-$here/build/release/harbor.duckdb_extension}

pass=0; fail=0; skipped=0
declare -a failures=()
bold=$(tput bold 2>/dev/null || true); red=$(tput setaf 1 2>/dev/null || true)
green=$(tput setaf 2 2>/dev/null || true); dim=$(tput dim 2>/dev/null || true)
yellow=$(tput setaf 3 2>/dev/null || true); off=$(tput sgr0 2>/dev/null || true)
section() { printf '\n%s%s%s\n' "$bold" "$1" "$off"; }
ok()   { pass=$((pass+1)); printf '  %s✓%s %s%s\n' "$green" "$off" "$1" "${2:+ ${dim}(${2})${off}}"; }
bad()  { fail=$((fail+1)); failures+=("$1"); printf '  %s✗%s %s\n     %s%s%s\n' "$red" "$off" "$1" "$dim" "${2:-}" "$off"; }
skip() { skipped=$((skipped+1)); printf '  %s—%s %s %s(%s)%s\n' "$yellow" "$off" "$1" "$dim" "${2:-}" "$off"; }
eq()   { if [[ "$2" == "$3" ]]; then ok "$1" "$3"; else bad "$1" "expected [$2], got [$3]"; fi; }

[[ -f "$ext" ]] || { echo "abi: no extension at $ext (run 'make release')" >&2; exit 2; }

# field <n> — the NUL-trimmed contents of FIELDn
field() {
  python3 - "$ext" "$1" <<'PY'
import sys
data = open(sys.argv[1], "rb").read()
n = int(sys.argv[2])
# FIELD1 ends 256 bytes from the end; each lower field sits 32 bytes earlier.
end = len(data) - 256 - (n - 1) * 32
print(data[end - 32:end].rstrip(b"\x00").decode("ascii", "replace"))
PY
}

# ---------------------------------------------------------------------------
section "The metadata footer"
# ---------------------------------------------------------------------------

# The loader looks for this before anything else; without it the file is just
# a shared library and DuckDB refuses it.
if python3 -c 'import sys; sys.exit(0 if b"duckdb_signature" in open(sys.argv[1],"rb").read()[-1024:] else 1)' "$ext"; then
  ok "carries the duckdb_signature marker"
else
  bad "carries the duckdb_signature marker" "no signature in the last 1024 bytes — this is not a loadable extension"
fi

eq "FIELD1 is the extension magic" "4" "$(field 1)"

want_platform=$(cat "$here/configure/platform.txt" 2>/dev/null || echo "?")
eq "FIELD2 is the configured platform" "$want_platform" "$(field 2)"

want_version=$(grep -m1 '^TARGET_DUCKDB_VERSION=' "$here/Makefile" | cut -d= -f2)
eq "FIELD3 is the DuckDB version from the Makefile" "$want_version" "$(field 3)"

want_ext_version=$(cat "$here/configure/extension_version.txt" 2>/dev/null || echo "?")
eq "FIELD4 is the extension version" "$want_ext_version" "$(field 4)"

# The one that regressed silently. USE_UNSTABLE_C_API=1 appends
# --abi-type C_STRUCT_UNSTABLE; without it the script's own default is
# C_STRUCT. Assert the artifact agrees with the Makefile, in both directions,
# so flipping the flag and having it not take effect is a failure rather than
# a surprise weeks later.
use_unstable=$(grep -m1 '^USE_UNSTABLE_C_API=' "$here/Makefile" | cut -d= -f2)
want_abi=$([[ "$use_unstable" == "1" ]] && echo C_STRUCT_UNSTABLE || echo C_STRUCT)
eq "FIELD5 is the ABI type the Makefile asks for" "$want_abi" "$(field 5)"

# ---------------------------------------------------------------------------
section "The C API version harbor requests at load"
# ---------------------------------------------------------------------------

# duckdb-rs's entrypoint macro passes a version string to get_api(); the host
# returns its API struct only if it can satisfy that version. It is a
# compile-time constant in a dependency, so a crate bump can change what this
# binary asks for without a line of harbor changing. Pin it here.
macro_default=$(grep -m1 'DEFAULT_DUCKDB_VERSION' \
  ~/.cargo/registry/src/*/duckdb-loadable-macros-*/src/lib.rs 2>/dev/null \
  | sed 's/.*= *"//; s/".*//')
if [[ -n "$macro_default" ]]; then
  eq "duckdb-rs requests the expected C API version" "v1.2.0" "$macro_default"
else
  skip "duckdb-rs requests the expected C API version" "macro crate not in the cargo registry"
fi

# harbor cannot run a query without these three, and all three live in the
# UNSTABLE band of the C API header. That is the real reason the build stamps
# C_STRUCT_UNSTABLE — not the Makefile flag, which is only how it is spelled.
# If a duckdb-rs release ever stops needing them, this check starts failing and
# the stable-ABI question is worth reopening.
sys_dir=$(ls -d ~/.cargo/registry/src/*/libduckdb-sys-*/ 2>/dev/null | head -1)
if [[ -n "$sys_dir" && -f "$sys_dir/wrapper_ext.h" ]]; then
  if grep -q 'DUCKDB_EXTENSION_API_VERSION_UNSTABLE' "$sys_dir/wrapper_ext.h"; then
    ok "duckdb-rs still compiles against the unstable C API surface" \
       "so C_STRUCT_UNSTABLE is required, not incidental"
  else
    bad "duckdb-rs still compiles against the unstable C API surface" \
        "wrapper_ext.h no longer defines it — a stable-ABI build may now be possible, which would make harbor loadable across DuckDB versions. Worth acting on."
  fi
else
  skip "duckdb-rs still compiles against the unstable C API surface" "libduckdb-sys not in the cargo registry"
fi

# ---------------------------------------------------------------------------
section "What the stamp costs: loading into a different engine"
# ---------------------------------------------------------------------------

# Point ALT_DUCKDB at a DuckDB binary of a *different* version. Skipped rather
# than failed when there isn't one, the same way differential.sh skips without
# the v1 build: a second engine is not a build dependency of this repository.
alt=${ALT_DUCKDB:-}
if [[ -z "$alt" ]]; then
  for candidate in "$here/../duckdb-v2/duckdb" "$here/../duckdb-alt/duckdb"; do
    [[ -x "$candidate" ]] && { alt="$candidate"; break; }
  done
fi

if [[ -z "$alt" || ! -x "$alt" ]]; then
  skip "loading into a different DuckDB engine" "no alternate engine; set ALT_DUCKDB=/path/to/duckdb"
else
  alt_version=$("$alt" -noheader -list -c 'SELECT version()' 2>/dev/null | tail -1)
  out=$("$alt" -unsigned -noheader -list -c "LOAD '$ext'; SELECT 1 AS ok;" 2>&1)
  loaded=$(grep -c '^1$' <<<"$out")

  if [[ "$want_abi" == "C_STRUCT_UNSTABLE" ]]; then
    # Pinned by design. The requirement is that it is refused, and refused with
    # something a person can read — not a hang, not a segfault.
    if (( loaded > 0 )); then
      bad "a version-pinned build is refused by $alt_version" \
          "it loaded — the stamp says C_STRUCT_UNSTABLE but the engine accepted it anyway"
    elif grep -qi 'version\|abi\|not compatible\|was built for' <<<"$out"; then
      ok "a version-pinned build is refused, with a version error" "$alt_version"
    else
      bad "a version-pinned build is refused, with a version error" \
          "refused, but the message does not mention the version mismatch: $(head -c 160 <<<"$out")"
    fi
  else
    # Stamped forward-loadable, so it has to actually work — and not merely
    # load. A load that succeeds and then cannot answer a query means the API
    # struct resolved to null function pointers.
    if (( loaded > 0 )); then
      ok "a stable-ABI build loads and answers on $alt_version"
      real=$("$alt" -unsigned -noheader -list \
             -c "LOAD '$ext'; SELECT count(*) FROM harbor_version();" 2>&1 | tail -1)
      eq "and its table functions work there" "1" "$real"
    else
      bad "a stable-ABI build loads and answers on $alt_version" "$(head -c 200 <<<"$out")"
    fi
  fi
fi

printf '\n%s%d passed, %d failed, %d skipped%s\n' "$bold" "$pass" "$fail" "$skipped" "$off"
if (( fail > 0 )); then
  printf '\n%sfailures:%s\n' "$red" "$off"
  for f in "${failures[@]}"; do printf '  - %s\n' "$f"; done
  exit 1
fi
exit 0
