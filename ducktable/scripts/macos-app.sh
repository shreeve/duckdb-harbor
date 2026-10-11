#!/bin/sh
# Assembles DuckTable.app so macOS gives the app its own icon and identity
# instead of attributing the bare binary to whatever terminal launched it,
# embeds Sparkle for in-app updates (docs/UPDATES.md), and signs it.
# Usage: scripts/macos-app.sh [debug|release]   (default: debug)
#
# Every build, local ones included, is signed with the Developer ID (SIGN
# names another identity; SIGN=- signs ad hoc, for a Mac without the
# certificate), with the hardened runtime that notarization requires. A
# release build also signs with a secure timestamp, which notarization
# requires and which needs the network. scripts/release.sh notarizes it.
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

# One name in the plist and in the signature. macOS files an app's privacy
# decisions — Local Network among them — under its signing identifier, so
# the identifier is the same on every build and one row in System Settings
# follows DuckTable across updates.
bundle_id="com.shreeve.ducktable"

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

# The kinds of file DuckTable opens (docs/OPEN.md), so Finder offers it for
# them and hands it the paths: double-click, Open With, a drop on the Dock
# icon, `open -a`. It owns DuckDB's own files. For files other apps open
# too, it ranks Alternate (CSV, JSON, Excel and the rest stay with the app
# that has them) or Default where the system knows no type at all; a folder
# is a folder of Parquet. Types the system does not declare are imported
# here, by their extensions. The crate's `open::tests` checks this list
# against the kinds DuckTable opens.
doc_type() { # name role rank uti...
    name=$1 role=$2 rank=$3
    shift 3
    printf '        <dict>\n            <key>CFBundleTypeName</key><string>%s</string>\n' "$name"
    printf '            <key>CFBundleTypeRole</key><string>%s</string>\n' "$role"
    printf '            <key>LSHandlerRank</key><string>%s</string>\n' "$rank"
    printf '            <key>LSItemContentTypes</key><array>'
    for uti in "$@"; do printf '<string>%s</string>' "$uti"; done
    printf '</array>\n        </dict>\n'
}
imported() { # uti description extension...
    uti=$1 description=$2
    shift 2
    printf '        <dict>\n            <key>UTTypeIdentifier</key><string>%s</string>\n' "$uti"
    printf '            <key>UTTypeDescription</key><string>%s</string>\n' "$description"
    printf '            <key>UTTypeConformsTo</key><array><string>public.data</string></array>\n'
    printf '            <key>UTTypeTagSpecification</key><dict><key>public.filename-extension</key><array>'
    for ext in "$@"; do printf '<string>%s</string>' "$ext"; done
    printf '</array></dict>\n        </dict>\n'
}
document_types=$(
    doc_type "DuckDB Database" Editor Owner org.duckdb.database
    doc_type "SQLite Database" Editor Alternate org.sqlite.database
    doc_type "CSV" Viewer Alternate public.comma-separated-values-text public.tab-separated-values-text
    doc_type "JSON" Viewer Alternate public.json public.ndjson org.jsonlines.jsonl
    doc_type "Excel Workbook" Viewer Alternate org.openxmlformats.spreadsheetml.sheet
    doc_type "Parquet" Viewer Default org.apache.parquet
    doc_type "GeoJSON" Viewer Alternate public.geojson
    doc_type "Geospatial Data" Viewer Default com.esri.shapefile org.opengeospatial.geopackage
    doc_type "Folder of Parquet" Viewer Alternate public.folder
)
imported_types=$(
    imported org.duckdb.database "DuckDB database" duckdb ddb
    imported org.sqlite.database "SQLite database" sqlite sqlite3 db
    imported org.apache.parquet "Apache Parquet file" parquet
    imported org.jsonlines.jsonl "JSON Lines" jsonl
    imported com.esri.shapefile "Esri shapefile" shp
    imported org.opengeospatial.geopackage "GeoPackage" gpkg
)

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>DuckTable</string>
    <key>CFBundleDisplayName</key><string>DuckTable</string>
    <key>CFBundleIdentifier</key><string>$bundle_id</string>
    <key>CFBundleExecutable</key><string>ducktable</string>
    <key>CFBundleIconFile</key><string>AppIcon</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>LSMinimumSystemVersion</key><string>12.0</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSLocalNetworkUsageDescription</key><string>DuckTable opens SSH tunnels to Harbor database servers on the hosts you add, which can be on your local network.</string>
    <key>CFBundleDocumentTypes</key>
    <array>
$document_types
    </array>
    <key>UTImportedTypeDeclarations</key>
    <array>
$imported_types
    </array>
$sparkle_keys
</dict>
</plist>
PLIST
plutil -lint "$app/Contents/Info.plist" >/dev/null

# Finder info and resource forks on copied files make codesign reject the
# bundle as "detritus"; strip extended attributes before signing. Sparkle's
# nested executables sign first, then the framework, then the app, all with
# one identity: the hardened runtime's library validation loads the
# framework only from the app's own team. Ad-hoc signatures have no team,
# so they go without the hardened runtime. Sparkle's code keeps Sparkle's
# identifiers; only the app is named here, and signing the app is what
# signs Contents/MacOS/ducktable.
sign="${SIGN:-Developer ID Application: Steve Shreeve (SD6N7Z8P9P)}"
set -- --force --sign "$sign"
[ "$sign" = "-" ] || set -- "$@" --options=runtime
[ "$sign" = "-" ] || [ "$profile" != "release" ] || set -- "$@" --timestamp
xattr -cr "$app"
codesign "$@" "$framework/Versions/B/Autoupdate"
codesign "$@" "$framework/Versions/B/Updater.app"
codesign "$@" "$framework"
codesign "$@" --identifier "$bundle_id" "$app"
codesign --verify --deep --strict "$app"

# The build fails rather than ship under another name: a bare executable
# signed without an identifier takes a hash-suffixed one, and macOS would
# treat that as a different app asking for the local network again.
for signed in "$app" "$app/Contents/MacOS/ducktable"; do
    got=$( (codesign -dv "$signed" 2>&1 || true) | sed -n 's/^Identifier=//p')
    [ "$got" = "$bundle_id" ] || {
        echo "error: $signed is signed as '${got:-nothing}', not $bundle_id" >&2
        exit 1
    }
done

echo "$app"
