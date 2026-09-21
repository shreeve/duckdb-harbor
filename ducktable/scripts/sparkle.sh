# Sparkle, pinned. Sourced by macos-app.sh (which embeds the framework) and
# appcast.sh (which signs releases with the same distribution's bin/ tools),
# so both come from one archive cached outside target/ where `cargo clean`
# cannot evict it. Bump the version and checksum together.
#
# Sourcing sets $sparkle_dir; ensure_sparkle populates it on first use.
sparkle_version="2.10.0"
sparkle_sha256="c2bf58aa8387266ac179357b1415d6f2635f044da8be41042af32425dae6da0c"
sparkle_cache_root=".ducktable-cache/sparkle"
sparkle_dir="$sparkle_cache_root/$sparkle_version"

ensure_sparkle() {
    [ -d "$sparkle_dir/Sparkle.framework" ] && return 0
    staging="$sparkle_cache_root/.staging-$sparkle_version-$$"
    rm -rf "$staging"
    mkdir -p "$staging"
    archive="$staging/Sparkle-$sparkle_version.tar.xz"
    curl -fsSL --retry 3 -o "$archive" \
        "https://github.com/sparkle-project/Sparkle/releases/download/$sparkle_version/Sparkle-$sparkle_version.tar.xz"
    echo "$sparkle_sha256  $archive" | shasum -a 256 -c - >/dev/null
    tar -xJf "$archive" -C "$staging" ./Sparkle.framework ./bin
    rm "$archive"
    mv "$staging" "$sparkle_dir"
}
