#!/usr/bin/env bash
# Hyperpanes — macOS .app bundle + .dmg packaging (track T6 packaging-macos).
#
# Contract (docs/ports-seams.md §3, frozen — release-rust.yml calls this blind):
#   rs/packaging/macos/bundle.sh <version>   # <version> WITHOUT a leading "v"
#   -> rs/packaging/out/hyperpanes-<version>.dmg
# Runs from any cwd (resolves the repo root from its own location), exits
# non-zero on any failure, puts ALL artifacts under rs/packaging/out/.
#
# Must work both on the Mac mini and on a GitHub macos-latest (arm64) runner:
# only stock tools are used (cargo, sips, iconutil, hdiutil, plutil).
#
# The bundle is unsigned — see README.md in this directory for the Gatekeeper
# quarantine note end users need.
set -euo pipefail

VERSION="${1:-}"
if [[ -z "$VERSION" ]]; then
    echo "usage: bundle.sh <version>   (e.g. bundle.sh 0.1.0 — no leading 'v')" >&2
    exit 2
fi
if [[ "$VERSION" == v* ]]; then
    echo "error: <version> must not carry a leading 'v' (got '$VERSION')" >&2
    exit 2
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"          # repo root (rs/packaging/macos -> ../../..)
OUT="$ROOT/rs/packaging/out"
STAGE="$OUT/macos-stage"                            # scratch; recreated every run
APP="$STAGE/Hyperpanes.app"
DMG="$OUT/hyperpanes-$VERSION.dmg"

echo "==> repo root: $ROOT"
echo "==> building rs/crates/app (release)"
cargo build --release --manifest-path "$ROOT/rs/crates/app/Cargo.toml" -j 4

# The app crate is NOT a workspace member: depending on local config the release
# binary lands either in the crate-local target dir or a shared rs/target.
BIN=""
for c in "$ROOT/rs/crates/app/target/release/hyperpanes" "$ROOT/rs/target/release/hyperpanes" "$ROOT/target/release/hyperpanes"; do
    if [[ -x "$c" ]]; then BIN="$c"; break; fi
done
if [[ -z "$BIN" ]]; then
    echo "error: release binary 'hyperpanes' not found under any known target dir" >&2
    exit 1
fi
echo "==> binary: $BIN"

echo "==> assembling Hyperpanes.app"
rm -rf "$STAGE"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/hyperpanes"
chmod 755 "$APP/Contents/MacOS/hyperpanes"

# Shell-integration scripts (cwd OSC -> project tint / clickable paths; mirrors
# what installer.nsi ships next to hyperpanes.exe on Windows).
# core::shell_integration::shell_integration_dir() resolves, relative to the
# RUNNING BINARY: exe_dir/resources/shell-integration, then exe_dir/shell-integration.
# In a bundle exe_dir is Contents/MacOS, so the copy the app actually finds today
# lives under Contents/MacOS/resources/. The Contents/Resources copy is where a
# bundle-aware lookup (exe_dir/../Resources) would expect it — shipped too so a
# future one-line core fix needs no packaging change.
for d in "$APP/Contents/MacOS/resources/shell-integration" "$APP/Contents/Resources/shell-integration"; do
    mkdir -p "$d"
    cp "$ROOT/resources/shell-integration/hp-init.ps1" "$d/"
    cp "$ROOT/resources/shell-integration/hp-init.sh" "$d/"
done

echo "==> generating hyperpanes.icns from build/icon.png"
# Source icon is 512x512 (build/icon.png — same art as the Windows icon.ico).
# Standard iconset, every size derived with sips; 512@2x needs a 1024 source so
# it is omitted (allowed — iconutil only requires the sizes present to be valid).
ICONSET="$STAGE/hyperpanes.iconset"
mkdir -p "$ICONSET"
SRC_ICON="$ROOT/build/icon.png"
sips -z 16 16     "$SRC_ICON" --out "$ICONSET/icon_16x16.png"      >/dev/null
sips -z 32 32     "$SRC_ICON" --out "$ICONSET/icon_16x16@2x.png"   >/dev/null
sips -z 32 32     "$SRC_ICON" --out "$ICONSET/icon_32x32.png"      >/dev/null
sips -z 64 64     "$SRC_ICON" --out "$ICONSET/icon_32x32@2x.png"   >/dev/null
sips -z 128 128   "$SRC_ICON" --out "$ICONSET/icon_128x128.png"    >/dev/null
sips -z 256 256   "$SRC_ICON" --out "$ICONSET/icon_128x128@2x.png" >/dev/null
sips -z 256 256   "$SRC_ICON" --out "$ICONSET/icon_256x256.png"    >/dev/null
sips -z 512 512   "$SRC_ICON" --out "$ICONSET/icon_256x256@2x.png" >/dev/null
sips -z 512 512   "$SRC_ICON" --out "$ICONSET/icon_512x512.png"    >/dev/null
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/hyperpanes.icns"

echo "==> writing Info.plist"
# CFBundleVersion must be period-separated numbers; strip any prerelease suffix
# (0.1.0-test -> 0.1.0). The full string stays in CFBundleShortVersionString.
BUNDLE_VERSION="${VERSION%%-*}"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>      <string>com.hyperpanes.app</string>
    <key>CFBundleName</key>            <string>Hyperpanes</string>
    <key>CFBundleDisplayName</key>     <string>Hyperpanes</string>
    <key>CFBundleExecutable</key>      <string>hyperpanes</string>
    <key>CFBundlePackageType</key>     <string>APPL</string>
    <key>CFBundleIconFile</key>        <string>hyperpanes</string>
    <key>CFBundleVersion</key>         <string>$BUNDLE_VERSION</string>
    <key>CFBundleShortVersionString</key> <string>$VERSION</string>
    <key>LSMinimumSystemVersion</key>  <string>11.0</string>
    <key>NSHighResolutionCapable</key> <true/>
    <key>CFBundleDocumentTypes</key>
    <array>
        <dict>
            <key>CFBundleTypeName</key>       <string>Hyperpanes Workspace</string>
            <key>CFBundleTypeRole</key>       <string>Editor</string>
            <key>LSHandlerRank</key>          <string>Owner</string>
            <key>CFBundleTypeIconFile</key>   <string>hyperpanes</string>
            <key>LSItemContentTypes</key>
            <array>
                <string>com.hyperpanes.workspace</string>
            </array>
        </dict>
    </array>
    <key>UTExportedTypeDeclarations</key>
    <array>
        <dict>
            <key>UTTypeIdentifier</key>   <string>com.hyperpanes.workspace</string>
            <key>UTTypeDescription</key>  <string>Hyperpanes Workspace</string>
            <key>UTTypeConformsTo</key>
            <array>
                <string>public.json</string>
            </array>
            <key>UTTypeTagSpecification</key>
            <dict>
                <key>public.filename-extension</key>
                <array>
                    <string>hyperpanes</string>
                </array>
            </dict>
        </dict>
    </array>
</dict>
</plist>
PLIST
plutil -lint "$APP/Contents/Info.plist"

echo "==> creating dmg"
DMG_STAGE="$STAGE/dmg-root"
mkdir -p "$DMG_STAGE"
cp -R "$APP" "$DMG_STAGE/"
ln -s /Applications "$DMG_STAGE/Applications"
rm -f "$DMG"
hdiutil create -volname "Hyperpanes" -srcfolder "$DMG_STAGE" -ov -format UDZO "$DMG"

echo "==> done: $DMG"
