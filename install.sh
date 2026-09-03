#!/usr/bin/env bash
#
# install.sh — install harbor with one command (macOS and Linux):
#
#   curl -fsSL https://raw.githubusercontent.com/shreeve/duckdb-harbor/main/install.sh | bash
#
# Pin a version by passing a tag (with or without the leading v):
#
#   curl -fsSL .../install.sh | bash -s v0.18.0
#
# Downloads the release archive for this platform, verifies its sha256 against
# the published checksums, and runs the archive's own installer — the binary
# to ~/.local/bin and libduckdb to ~/.local/lib, both overridable with BIN=
# and LIB=. Nothing here needs root.
#
# Uninstall the same way — the binary and libduckdb go; your databases,
# state, and config stay:
#
#   curl -fsSL .../install.sh | bash -s -- --uninstall
#
# On Windows use install.ps1 instead:
#
#   irm https://raw.githubusercontent.com/shreeve/duckdb-harbor/main/install.ps1 | iex

set -euo pipefail

REPO=shreeve/duckdb-harbor
NAME=harbor

# Color only when stdout is a terminal, and never against NO_COLOR.
Color_Off='' Red='' Green='' Dim='' Bold_Green='' Bold_White=''
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
  Color_Off='\033[0m'
  Red='\033[0;31m' Green='\033[0;32m' Dim='\033[0;2m'
  Bold_Green='\033[1;32m' Bold_White='\033[1m'
fi

info() { printf "${Dim}%s${Color_Off}\n" "$*"; }
fail() { printf "${Red}error${Color_Off}: %s\n" "$*" >&2; exit 1; }
tildify() { case "$1" in "$HOME"/*) printf '~%s' "${1#"$HOME"}" ;; *) printf '%s' "$1" ;; esac; }

# Remove only what install put down — the binary and the engine. Databases,
# state (the fleet's sockets die with their servers), and config belong to
# the user, and an uninstaller that reaches for those is malware with a
# manual. No network: what is installed is answered by the filesystem.
uninstall() {
  BIN=${BIN:-$HOME/.local/bin}
  LIB=${LIB:-$HOME/.local/lib}
  if [ ! -e "$BIN/$NAME" ] && [ ! -e "$LIB/libduckdb.dylib" ] && [ ! -e "$LIB/libduckdb.so" ]; then
    fail "$NAME is not installed at $(tildify "$BIN/$NAME") (BIN=/LIB= if it lives elsewhere)"
  fi
  if [ -e "$BIN/$NAME" ]; then
    rm -f "$BIN/$NAME" || fail "cannot remove $(tildify "$BIN/$NAME") — re-run under sudo if it was installed system-wide"
    printf "${Green}$NAME was removed from ${Bold_Green}%s${Color_Off}\n" "$(tildify "$BIN")"
  fi
  removed_lib=false
  for lib in "$LIB"/libduckdb.dylib "$LIB"/libduckdb.so; do
    if [ -e "$lib" ]; then
      rm -f "$lib" || fail "cannot remove $(tildify "$lib")"
      removed_lib=true
    fi
  done
  if $removed_lib; then
    printf "${Green}libduckdb was removed from ${Bold_Green}%s${Color_Off}\n" "$(tildify "$LIB")"
  fi
  info "your databases, state ($(tildify "${XDG_STATE_HOME:-$HOME/.local/state}/harbor")), and config ($(tildify "${XDG_CONFIG_HOME:-$HOME/.config}/harbor")) are untouched"
}

# Everything lives in main() so a truncated `curl | bash` download can
# never execute a half-delivered script.
main() {
  case "${1:-}" in
    --uninstall) uninstall; return ;;
  esac

  command -v curl >/dev/null || fail "curl is required"
  command -v tar  >/dev/null || fail "tar is required"

  # --- platform -> release asset suffix ------------------------------------
  os=$(uname -s) arch=$(uname -m)
  # A shell running under Rosetta reports x86_64 on Apple Silicon.
  if [ "$os" = Darwin ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
    arch=arm64
  fi
  case "$os-$arch" in
    Darwin-arm64)          plat=osx-arm64    ;;
    Linux-x86_64)          plat=linux-amd64  ;;
    Linux-aarch64|Linux-arm64) plat=linux-arm64 ;;
    Darwin-x86_64)         fail "no Intel macOS build is published (Apple Silicon only)" ;;
    MINGW*|MSYS*|CYGWIN*)  fail "on Windows use install.ps1: irm https://raw.githubusercontent.com/$REPO/main/install.ps1 | iex" ;;
    *)                     fail "unsupported platform: $os $arch" ;;
  esac

  # --- version: argument, or the tag `releases/latest` redirects to --------
  tag=${1:-}
  if [ -n "$tag" ]; then
    case "$tag" in v*) ;; *) tag="v$tag" ;; esac
  else
    tag=$(curl -fsSLI --retry 3 --retry-delay 1 -o /dev/null -w '%{url_effective}' \
      "https://github.com/$REPO/releases/latest") || fail "cannot reach github.com"
    tag=${tag##*/}
  fi
  # With no releases, GitHub redirects .../latest to .../releases — so the
  # resolved "tag" is only real if it looks like one.
  case "$tag" in v*) ;; *) fail "no releases found for $REPO" ;; esac

  asset="$NAME-$tag-$plat.tar.gz"
  base="https://github.com/$REPO/releases/download/$tag"

  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT

  info "$NAME $tag ($plat)"
  curl -fSL --retry 3 --retry-delay 1 --progress-bar -o "$tmp/$asset" "$base/$asset" \
    || fail "download failed: $base/$asset"

  # --- verify against the release's published checksums --------------------
  curl -fsSL --retry 3 --retry-delay 1 -o "$tmp/checksums.txt" "$base/$NAME-$tag-checksums.txt" \
    || fail "download failed: $NAME-$tag-checksums.txt"
  if command -v sha256sum >/dev/null; then
    sum=$(sha256sum "$tmp/$asset" | cut -d' ' -f1)
  else
    sum=$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1)
  fi
  want=$(awk -v f="$asset" '$2 == f { print $1 }' "$tmp/checksums.txt")
  [ -n "$want" ]        || fail "no checksum published for $asset"
  [ "$sum" = "$want" ]  || fail "checksum mismatch for $asset"

  # --- extract and hand off to the archive's own installer ------------------
  tar -xzf "$tmp/$asset" -C "$tmp"
  bash "$tmp/$NAME-$tag-$plat/install.sh"
}

main "$@"
