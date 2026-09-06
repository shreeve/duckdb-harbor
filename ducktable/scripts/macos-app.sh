#!/bin/sh
# Assembles DuckTable.app so macOS gives the app its own icon and identity
# instead of attributing the bare binary to whatever terminal launched it,
# embeds Sparkle for in-app updates (docs/UPDATES.md), and ad-hoc signs the
# result so Sparkle can verify what it installs.
# Usage: scripts/macos-app.sh [debug|release]   (default: debug)
set -e
cd "$(dirname "$0")/.."

profile="${1:-debug}"
if [ "$profile" = "release" ]; then
    cargo build --release
else
    cargo build
fi

# The bundle says the workspace's version, not a hardcoded one — About
# and Finder must never disagree with the binary they describe. It is also
# the build number: Sparkle orders updates by CFBundleVersion, so that has
# to change every release, and the dotted version already does.
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

# The public half of the update-signing key (assets/sparkle-public-key.txt,
# from `generate_keys --account ducktable -p`). Without it Sparkle refuses
# to start and the app simply has no Check for Updates item — a bundle that
# runs, not one that lies about updating.
feed_url="https://github.com/shreeve/duckdb-harbor/releases/download/ducktable-updates/appcast.xml"
public_key=""
if [ -f assets/sparkle-public-key.txt ]; then
    public_key=$(tr -d '[:space:]' < assets/sparkle-public-key.txt)
fi
if [ -n "$public_key" ]; then
    sparkle_keys="    <key>SUFeedURL</key><string>$feed_url</string>
    <key>SUPublicEDKey</key><string>$public_key</string>"
else
    sparkle_keys=""
    echo "note: assets/sparkle-public-key.txt is missing; the bundle will not check for updates (docs/UPDATES.md)" >&2
fi

. scripts/sparkle.sh
ensure_sparkle

app="target/DuckTable.app"
frameworks="$app/Contents/Frameworks"
framework="$frameworks/Sparkle.framework"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$frameworks"
cp "target/$profile/ducktable" "$app/Contents/MacOS/ducktable"
cp assets/AppIcon.icns "$app/Contents/Resources/AppIcon.icns"
cp -R "$sparkle_dir/Sparkle.framework" "$framework"
# DuckTable is not sandboxed, so Sparkle's XPC services never run; drop them
# with the header and module folders so the shipped framework carries no dev
# artifacts and no unsigned nested code.
for extra in XPCServices Headers PrivateHeaders Modules; do
    rm -rf "$framework/$extra" "$framework/Versions/B/$extra"
done

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>DuckTable</string>
    <key>CFBundleDisplayName</key><string>DuckTable</string>
    <key>CFBundleIdentifier</key><string>com.shreeve.ducktable</string>
    <key>CFBundleExecutable</key><string>ducktable</string>
    <key>CFBundleIconFile</key><string>AppIcon</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>LSMinimumSystemVersion</key><string>12.0</string>
    <key>NSHighResolutionCapable</key><true/>
$sparkle_keys
</dict>
</plist>
PLIST

# Finder info and resource forks on copied files make codesign reject the
# bundle as "detritus"; strip extended attributes before signing. Sparkle's
# nested executables sign first, then the framework, then the app, all
# ad-hoc: Sparkle verifies an update's signature is valid, and ad-hoc is.
xattr -cr "$app"
codesign --force --sign - "$framework/Versions/B/Autoupdate"
codesign --force --sign - "$framework/Versions/B/Updater.app"
codesign --force --sign - "$framework"
codesign --force --sign - "$app"
codesign --verify --deep --strict "$app"

echo "$app"
