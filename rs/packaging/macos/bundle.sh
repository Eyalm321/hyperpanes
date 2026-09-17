#!/usr/bin/env bash
# Avada — macOS .app bundle + .dmg packaging (track T6 packaging-macos).
#
# Contract (docs/ports-seams.md §3, frozen — release-rust.yml calls this blind):
#   rs/packaging/macos/bundle.sh <version>   # <version> WITHOUT a leading "v"
#   -> rs/packaging/out/avada-<version>.dmg
# Runs from any cwd (resolves the repo root from its own location), exits
# non-zero on any failure, puts ALL artifacts under rs/packaging/out/.
#
# Must work both on the Mac mini and on a GitHub macos-latest (arm64) runner:
# only stock tools are used (cargo, sips, iconutil, hdiutil, plutil, codesign).
#
# Signing and notarization are both opt-in through the environment, and both
# degrade to a warning rather than a failure, so this stays runnable on a machine
# with no certificate at all:
#
#   AVADA_SIGN_ID        codesign identity. Defaults to the first
#                             "Developer ID Application" in the login keychain,
#                             then to ad-hoc ("-") with a warning.
#   AVADA_NOTARY_PROFILE `xcrun notarytool store-credentials` profile name.
#                             Unset -> notarization is skipped.
#
# An ad-hoc signature is not nothing: it is what lets macOS attach TCC grants to
# the app at all. It just changes on every rebuild, so the grants do too — which
# is exactly the problem a real Developer ID solves.
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

# Fail before doing any work if someone has exported a password expecting this
# script to pick it up. It never will: notarization authenticates only through a
# keychain profile, so a password in the environment is a misunderstanding worth
# stopping for rather than silently ignoring.
for leaked in AVADA_NOTARY_PASSWORD APPLE_APP_SPECIFIC_PASSWORD AC_PASSWORD; do
    if [[ -n "${!leaked:-}" ]]; then
        echo "error: $leaked is set. This script never accepts a password." >&2
        echo "       Run: xcrun notarytool store-credentials <profile-name>" >&2
        echo "       then set AVADA_NOTARY_PROFILE=<profile-name>." >&2
        exit 2
    fi
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"          # repo root (rs/packaging/macos -> ../../..)
OUT="$ROOT/rs/packaging/out"
STAGE="$OUT/macos-stage"                            # scratch; recreated every run
APP="$STAGE/Avada.app"
DMG="$OUT/avada-$VERSION.dmg"

echo "==> repo root: $ROOT"
echo "==> building rs/crates/app (release)"
cargo build --release --manifest-path "$ROOT/rs/crates/app/Cargo.toml" -j 4

# The app crate is NOT a workspace member: depending on local config the release
# binary lands either in the crate-local target dir or a shared rs/target.
BIN=""
for c in "$ROOT/rs/crates/app/target/release/avada" "$ROOT/rs/target/release/avada" "$ROOT/target/release/avada"; do
    if [[ -x "$c" ]]; then BIN="$c"; break; fi
done
if [[ -z "$BIN" ]]; then
    echo "error: release binary 'avada' not found under any known target dir" >&2
    exit 1
fi
echo "==> binary: $BIN"

echo "==> assembling Avada.app"
rm -rf "$STAGE"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/avada"
chmod 755 "$APP/Contents/MacOS/avada"

# Two lookups have to be satisfied at once, and they disagree about where a
# resource lives. Both resolve relative to the RUNNING BINARY, i.e. Contents/MacOS:
# core::shell_integration::shell_integration_dir() and submit_new_goal() want
# exe_dir/resources/..., while a bundle-aware lookup wants exe_dir/../Resources/....
#
# This used to be answered by shipping two copies. It cannot be any more: codesign
# treats every directory under Contents/MacOS as nested code and refuses to sign a
# bundle whose Contents/MacOS holds a SKILL.md ("code object is not signed at all").
# So the files live in Contents/Resources, where a signature seals them properly,
# and Contents/MacOS/resources is a relative symlink onto it — sealed as a symlink,
# never descended into. Both lookups land on the same bytes, and there is now only
# one set of them.
mkdir -p "$APP/Contents/Resources/shell-integration"
cp "$ROOT/resources/shell-integration/hp-init.ps1" "$APP/Contents/Resources/shell-integration/"
cp "$ROOT/resources/shell-integration/hp-init.sh" "$APP/Contents/Resources/shell-integration/"
cp -R "$ROOT/resources/shell-integration/zdotdir" "$APP/Contents/Resources/shell-integration/"

mkdir -p "$APP/Contents/Resources/claude/goal-orchestrator"
cp "$ROOT/resources/claude/goal-orchestrator/SKILL.md" "$APP/Contents/Resources/claude/goal-orchestrator/"
cp "$ROOT/resources/claude/goal-orchestrator/SPEC.md" "$APP/Contents/Resources/claude/goal-orchestrator/"
cp "$ROOT/resources/claude/goal-orchestrator/IMPL.md" "$APP/Contents/Resources/claude/goal-orchestrator/"

# The always-on Hyperpane tab's working directory (README + the hidden .claude/skills tree its
# agent loads). `cp -R` of the directory itself, so the dot-directory rides along — a glob would
# silently drop it and ship the tab an agent that can't drive anything.
mkdir -p "$APP/Contents/Resources/claude"
rm -rf "$APP/Contents/Resources/claude/hyperpane"
cp -R "$ROOT/resources/claude/hyperpane" "$APP/Contents/Resources/claude/hyperpane"

# CLI-agent session hooks (tool-resume feature). These had never been bundled at all, so a
# shipped .app registered no hook and every hand-started tool pane fell back to the
# scan-and-diff heuristic. They go in Contents/Resources for the same signing reason as the
# personas above, and are reached through the MacOS/resources symlink.
for h in claude/hp-claude-session-hook.sh cursor/hp-cursor-session-hook.sh copilot/hp-copilot-session-hook.sh \
         codex/hp-codex-session-hook.sh gemini/hp-gemini-session-hook.sh; do
  mkdir -p "$APP/Contents/Resources/$(dirname "$h")"
  install -m 755 "$ROOT/resources/$h" "$APP/Contents/Resources/$h"
done

# First-party modules, shipped precompiled so first run seeds them offline (requirement
# #2). stage_seed_modules builds each in-tree module and lays out
# Contents/Resources/seed-modules/<owner>__<repo>/{avada.toml,bin/<name>,skills...},
# which core::install::seed::seed_modules_dir() finds via the ../Resources rule.
# Reached through the MacOS/resources symlink like the other bundled resources; the
# binaries it drops in are nested Mach-O and are signed inside-out below.
source "$SCRIPT_DIR/../seed-modules.sh"
stage_seed_modules "$APP/Contents/Resources/seed-modules"

ln -s ../Resources "$APP/Contents/MacOS/resources"

echo "==> generating avada.icns from build/icon.png"
# Source icon is 512x512 (build/icon.png — same art as the Windows icon.ico).
# Standard iconset, every size derived with sips; 512@2x needs a 1024 source so
# it is omitted (allowed — iconutil only requires the sizes present to be valid).
ICONSET="$STAGE/avada.iconset"
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
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/avada.icns"

echo "==> writing Info.plist"
# The NS*UsageDescription strings below are the text macOS puts in the consent
# dialog, so they are written to be read by the person deciding, not by us.
# Screen Recording, Accessibility and Full Disk Access have no such key — Apple
# fixes their dialog text — which is why `core::permissions::request` deep-links
# into the Settings pane for those three instead.
# CFBundleVersion must be period-separated numbers; strip any prerelease suffix
# (0.1.0-test -> 0.1.0). The full string stays in CFBundleShortVersionString.
BUNDLE_VERSION="${VERSION%%-*}"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>      <string>to.avada.terminal</string>
    <key>CFBundleName</key>            <string>Avada</string>
    <key>CFBundleDisplayName</key>     <string>Avada Terminal</string>
    <key>CFBundleExecutable</key>      <string>avada</string>
    <key>CFBundlePackageType</key>     <string>APPL</string>
    <key>CFBundleIconFile</key>        <string>avada</string>
    <key>CFBundleVersion</key>         <string>$BUNDLE_VERSION</string>
    <key>CFBundleShortVersionString</key> <string>$VERSION</string>
    <key>LSMinimumSystemVersion</key>  <string>11.0</string>
    <key>NSHighResolutionCapable</key> <true/>
    <key>NSMicrophoneUsageDescription</key>
    <string>Avada uses the microphone only while you are dictating into a pane, and stops listening the moment you stop.</string>
    <key>NSAppleEventsUsageDescription</key>
    <string>Avada sends Apple events so a pane can hand a file or a link to the editor or browser you picked, instead of opening it here.</string>
    <key>NSDesktopFolderUsageDescription</key>
    <string>Avada needs this to open a workspace, or start a terminal, in a project you keep on your Desktop.</string>
    <key>NSDocumentsFolderUsageDescription</key>
    <string>Avada needs this to open a workspace, or start a terminal, in a project you keep in Documents.</string>
    <key>NSDownloadsFolderUsageDescription</key>
    <string>Avada needs this to open a workspace, or start a terminal, in a project you keep in Downloads.</string>
    <key>NSRemovableVolumesUsageDescription</key>
    <string>Avada needs this to open a workspace, or start a terminal, in a project you keep on an external drive.</string>
    <key>CFBundleDocumentTypes</key>
    <array>
        <dict>
            <key>CFBundleTypeName</key>       <string>Avada Workspace</string>
            <key>CFBundleTypeRole</key>       <string>Editor</string>
            <key>LSHandlerRank</key>          <string>Owner</string>
            <key>CFBundleTypeIconFile</key>   <string>avada</string>
            <key>LSItemContentTypes</key>
            <array>
                <string>to.avada.workspace</string>
            </array>
        </dict>
    </array>
    <key>UTExportedTypeDeclarations</key>
    <array>
        <dict>
            <key>UTTypeIdentifier</key>   <string>to.avada.workspace</string>
            <key>UTTypeDescription</key>  <string>Avada Workspace</string>
            <key>UTTypeConformsTo</key>
            <array>
                <string>public.json</string>
            </array>
            <key>UTTypeTagSpecification</key>
            <dict>
                <key>public.filename-extension</key>
                <array>
                    <string>avada</string>
                </array>
            </dict>
        </dict>
    </array>
</dict>
</plist>
PLIST
plutil -lint "$APP/Contents/Info.plist"

# ---------------------------------------------------------------- code signing
# Identity resolution, in order: the environment, then the login keychain, then
# ad-hoc. Only the identity NAME is ever printed — it is not a secret, but the
# key behind it is, and nothing here reads, prompts for, or writes one. Signing
# happens entirely through the keychain, which is where the key stays.
#
# entitlements.plist carries no XML comments because AMFI, the parser codesign
# hands it to, rejects them outright ("syntax error near line 4"), so the reasons
# for its two keys live here instead.
#
# Both are hardened-runtime gates that close *before* TCC gets to ask. Without
# them macOS refuses the microphone and Apple events outright and the user never
# sees a dialog to say yes to; with them the user is still asked and still free
# to say no. They are the packaging half of core::permissions::Right::Microphone
# and Right::Automation.
#
# Deliberately absent: `allow-jit` and `allow-unsigned-executable-memory` —
# nothing in this build generates code at runtime (Metal via wgpu, the bundled
# SQLite amalgamation, no scripting engine), and turning off W^X in a process
# that hosts the user's shells needs a better reason than "just in case".
# `disable-library-validation` — the binary is statically linked and loads no
# dylibs of its own. `app-sandbox` — Avada runs the user's shells and opens
# the user's repositories, which is the opposite of a container; Full Disk Access
# is a grant the user makes in Settings, not an entitlement we can claim.
ENTITLEMENTS="$SCRIPT_DIR/entitlements.plist"
plutil -lint "$ENTITLEMENTS"

SIGN_ID="${AVADA_SIGN_ID:-}"
if [[ -z "$SIGN_ID" ]]; then
    # `find-identity` prints every usable identity; take the first Developer ID
    # Application line and nothing else. Apple Development / Apple Distribution
    # certs are deliberately not accepted: neither notarizes, and shipping one
    # would produce a build that looks signed and still fails Gatekeeper.
    SIGN_ID="$(security find-identity -v -p codesigning 2>/dev/null \
        | awk -F'"' '/"Developer ID Application/ { print $2; exit }')"
fi

SIGN_TIMESTAMP=(--timestamp)
if [[ -z "$SIGN_ID" ]]; then
    SIGN_ID="-"
    SIGN_TIMESTAMP=(--timestamp=none)   # ad-hoc has no identity to timestamp
    echo "" >&2
    echo "WARNING: no Developer ID Application certificate found — signing AD-HOC." >&2
    echo "WARNING: Gatekeeper will refuse this build on any Mac but this one, and" >&2
    echo "WARNING: macOS will drop every permission grant the moment it is rebuilt." >&2
    echo "WARNING: Set AVADA_SIGN_ID, or install a Developer ID, for a release." >&2
    echo "" >&2
else
    echo "==> signing identity: $SIGN_ID"
fi

echo "==> signing Avada.app (hardened runtime)"
# Nested Mach-O must be signed inside-out: the seeded module binaries under
# Contents/Resources/seed-modules/*/bin/* are separate executables, and a signature on
# the app that finds an unsigned one nested inside fails --verify --strict. So sign each
# module binary first (hardened runtime, no entitlements — they are headless RPC helpers
# that render nothing and need none of the app's exceptions), then seal the app over them.
# We avoid --deep (deprecated, signs blindly) in favour of signing the known set.
while IFS= read -r -d '' modbin; do
    codesign --force --options runtime "${SIGN_TIMESTAMP[@]}" --sign "$SIGN_ID" "$modbin"
done < <(find "$APP/Contents/Resources/seed-modules" -type f -path '*/bin/*' -perm -u+x -print0 2>/dev/null)

codesign --force --options runtime "${SIGN_TIMESTAMP[@]}" \
    --entitlements "$ENTITLEMENTS" \
    --sign "$SIGN_ID" "$APP"
codesign --verify --strict --verbose=2 "$APP"

echo "==> creating dmg"
DMG_STAGE="$STAGE/dmg-root"
mkdir -p "$DMG_STAGE"
# ditto, not cp: it is the copy that carries extended attributes across intact,
# and a signature that loses them is a signature that fails to verify.
ditto "$APP" "$DMG_STAGE/Avada.app"
ln -s /Applications "$DMG_STAGE/Applications"
rm -f "$DMG"
# The image size is given EXPLICITLY. `hdiutil create -srcfolder` otherwise sizes the
# volume from the folder's apparent bytes, which is not what lands on it: `ditto` carries
# the extended attributes across (that is why it is ditto), and those — plus the APFS
# metadata for a signed bundle's several thousand hashes — do not fit the estimate. It
# fails as "No space left on device" naming /Volumes/Avada, which is the mounted IMAGE and
# not the disk, so it reads like the machine is full when it has hundreds of gigabytes
# free. Payload plus half again, and never less than 128 MiB, so this does not come back
# the next time the bundle grows.
DMG_MB=$(du -sm "$DMG_STAGE" | cut -f1)
DMG_MB=$(( DMG_MB * 3 / 2 + 64 ))
if [ "$DMG_MB" -lt 128 ]; then DMG_MB=128; fi
hdiutil create -volname "Avada" -srcfolder "$DMG_STAGE" -ov -format UDZO \
    -size "${DMG_MB}m" "$DMG"
codesign --force "${SIGN_TIMESTAMP[@]}" --sign "$SIGN_ID" "$DMG"

# ---------------------------------------------------------------- notarization
# Opt-in and never on the ad-hoc path, where Apple would reject the submission
# anyway. Credentials come only from a keychain profile created out of band with
# `xcrun notarytool store-credentials <name>`; an Apple ID password or
# app-specific password must never reach this script, its arguments, or its
# environment, and the check below refuses to continue if one has been exported
# in the hope that it would; the guard for that is at the top of the script.

NOTARY_PROFILE="${AVADA_NOTARY_PROFILE:-}"
if [[ -z "$NOTARY_PROFILE" ]]; then
    echo "==> notarization skipped (AVADA_NOTARY_PROFILE unset)"
elif [[ "$SIGN_ID" == "-" ]]; then
    echo "==> notarization skipped (ad-hoc signature: Apple only notarizes Developer ID)"
elif ! xcrun --find notarytool >/dev/null 2>&1; then
    echo "==> notarization skipped (xcrun notarytool unavailable — needs Xcode 13+)"
else
    echo "==> notarizing $DMG as profile '$NOTARY_PROFILE' (this waits on Apple)"
    xcrun notarytool submit "$DMG" --keychain-profile "$NOTARY_PROFILE" --wait
    xcrun stapler staple "$DMG"
    xcrun stapler validate "$DMG"
fi

echo "==> done: $DMG"
