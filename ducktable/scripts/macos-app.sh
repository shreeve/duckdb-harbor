#!/bin/sh
# Assembles DuckTable.app so macOS gives the app its own icon and identity
# instead of attributing the bare binary to whatever terminal launched it.
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
# and Finder must never disagree with the binary they describe.
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

app="target/DuckTable.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "target/$profile/ducktable" "$app/Contents/MacOS/ducktable"
cp assets/AppIcon.icns "$app/Contents/Resources/AppIcon.icns"

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
    <key>CFBundleVersion</key><string>1</string>
    <key>LSMinimumSystemVersion</key><string>12.0</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

echo "$app"
