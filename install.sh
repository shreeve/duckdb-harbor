#!/usr/bin/env bash
#
# install.sh — install harbor + pilot with one command (macOS and Linux):
#
#   curl -fsSL https://raw.githubusercontent.com/shreeve/duckdb-harbor/main/install.sh | bash
#
# Pin a version by passing a tag (with or without the leading v):
#
#   curl -fsSL .../install.sh | bash -s v0.13.4
#
# Downloads the release archive for this platform, verifies its sha256
# against the published checksums, and runs the archive's own installer
# (binaries -> /usr/local/bin, libduckdb -> /usr/local/lib, ui extension
# -> ~/.duckdb; override with BIN=... LIB=...). sudo is used only if the
# destination dirs are root-owned. Windows support comes later.

set -euo pipefail

REPO=shreeve/duckdb-harbor
NAME=harbor

say()  { printf '%s\n' "$*"; }
fail() { printf 'install: %s\n' "$*" >&2; exit 1; }

# Everything lives in main() so a truncated `curl | bash` download can
# never execute a half-delivered script.
main() {
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
    MINGW*|MSYS*|CYGWIN*)  fail "Windows is not supported by this installer yet" ;;
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

  say "installing $NAME $tag ($plat)"
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

  # 0.14.0 moved harbor's home: config.toml at ~/.config/harbor, the live
  # fleet under ~/.config/harbor/runtime. Move a pre-0.14 ~/.harbor across
  # once — but never while berths may still be serving out of it.
  if [ "$(printf '%s\n' v0.14.0 "$tag" | sort -V | head -1)" = v0.14.0 ]; then
    root="${HARBOR_HOME:-$HOME/.config/harbor}"
    hh="$root/runtime"
    old="$HOME/.harbor"
    if [ -d "$old" ] && [ ! -d "$hh" ]; then
      if pgrep -x harbor >/dev/null 2>&1; then
        say "note: ~/.harbor was NOT migrated while harbor is running. Stop the"
        say "  fleet, then: mkdir -p $hh && mv $old/config.toml $root/; mv $old/* $hh/ && rmdir $old"
      else
        mkdir -p "$hh"
        [ -f "$old/config.toml" ] && mv "$old/config.toml" "$root/config.toml"
        find "$old" -mindepth 1 -maxdepth 1 -exec mv {} "$hh/" \;
        rmdir "$old" 2>/dev/null || true
        say "migrated ~/.harbor -> $root (config.toml at the root, fleet state in runtime/)"
      fi
    fi
  else
    hh="${HARBOR_HOME:-$HOME/.harbor}"   # pre-0.14 layout
  fi

  # Sockets and tokens live in the runtime dir; a dir made earlier by hand
  # (or a sloppy umask) must not stay world-listable.
  if [ -d "$hh" ]; then chmod 700 "$hh" 2>/dev/null || true; fi
}

main "$@"
