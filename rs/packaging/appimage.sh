#!/usr/bin/env bash
# Build the hyperpanes AppImage for x86_64 Linux.
#
# Contract (docs/ports-seams.md §3, frozen — release-rust.yml calls this blind):
#   rs/packaging/appimage.sh <version>     # <version> WITHOUT a leading "v"
#   → rs/packaging/out/hyperpanes-<version>-x86_64.AppImage
# Runs from any cwd; exits non-zero on any failure; all artifacts under
# rs/packaging/out/.
#
# Needs: bash, curl (or wget), cargo + a Linux x86_64 toolchain, and the usual
# Slint/winit native build deps (see rs/packaging/linux/README.md). appimagetool
# is downloaded (pinned) into a cache dir if not already present.

set -euo pipefail

err() { echo "appimage.sh: error: $*" >&2; exit 1; }

VERSION="${1:-}"
[ -n "$VERSION" ] || err "usage: appimage.sh <version>  (e.g. 0.0.6, no leading 'v')"
case "$VERSION" in v*) err "<version> must not have a leading 'v' (got '$VERSION')";; esac

ARCH_TRIPLE="x86_64"
[ "$(uname -m)" = "x86_64" ] || err "this script targets x86_64 (host is $(uname -m))"

# Resolve the repo root from this script's own location (rs/packaging/ → ../..).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
LINUX_DIR="$SCRIPT_DIR/linux"
OUT_DIR="$SCRIPT_DIR/out"
APP_MANIFEST="$ROOT/rs/crates/app/Cargo.toml"
[ -f "$APP_MANIFEST" ] || err "app manifest not found at $APP_MANIFEST"

mkdir -p "$OUT_DIR"

# --- 1. release build of the app crate (NON-member crate: --manifest-path) ----
echo "==> cargo build --release (rs/crates/app)"
cargo build --release --manifest-path "$APP_MANIFEST"

# The app crate has its own target dir unless CARGO_TARGET_DIR overrides it;
# ask cargo rather than guessing.
TARGET_DIR="${CARGO_TARGET_DIR:-$(cargo metadata --manifest-path "$APP_MANIFEST" \
  --format-version 1 --no-deps 2>/dev/null \
  | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')}"
[ -n "$TARGET_DIR" ] || err "could not determine cargo target directory"
BIN="$TARGET_DIR/release/hyperpanes"
[ -x "$BIN" ] || err "built binary not found at $BIN"

# --- 2. assemble the AppDir ---------------------------------------------------
APPDIR="$OUT_DIR/AppDir"
echo "==> assembling AppDir at $APPDIR"
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" \
         "$APPDIR/usr/share/applications" \
         "$APPDIR/usr/share/mime/packages"

install -m 755 "$BIN" "$APPDIR/usr/bin/hyperpanes"

# Shell-integration init scripts: the app resolves exe_dir/resources/shell-integration
# (same layout installer.nsi ships on Windows). ConPTY pair is Windows-only — skipped.
SHELL_INT="$ROOT/resources/shell-integration"
[ -d "$SHELL_INT" ] || err "missing $SHELL_INT"
mkdir -p "$APPDIR/usr/bin/resources/shell-integration"
install -m 644 "$SHELL_INT/hp-init.sh"  "$APPDIR/usr/bin/resources/shell-integration/hp-init.sh"
install -m 644 "$SHELL_INT/hp-init.ps1" "$APPDIR/usr/bin/resources/shell-integration/hp-init.ps1"
mkdir -p "$APPDIR/usr/bin/resources/shell-integration/zdotdir"
install -m 644 "$SHELL_INT/zdotdir/.zshenv" "$APPDIR/usr/bin/resources/shell-integration/zdotdir/.zshenv"
install -m 644 "$SHELL_INT/zdotdir/.zshrc"  "$APPDIR/usr/bin/resources/shell-integration/zdotdir/.zshrc"
# Claude Code session hook (claude-resume feature) — same exe_dir/resources layout.
mkdir -p "$APPDIR/usr/bin/resources/claude"
install -m 755 "$ROOT/resources/claude/hp-claude-session-hook.sh" "$APPDIR/usr/bin/resources/claude/hp-claude-session-hook.sh"

# Goal-orchestrator personas (goals system) — resolved as exe_dir/resources/claude/goal-orchestrator.
mkdir -p "$APPDIR/usr/bin/resources/claude/goal-orchestrator"
for f in SKILL.md SPEC.md IMPL.md; do
  install -m 644 "$ROOT/resources/claude/goal-orchestrator/$f" "$APPDIR/usr/bin/resources/claude/goal-orchestrator/$f"
done

# Desktop entry + MIME info (registered by appimaged/AppImageLauncher or a
# package manager hook via update-mime-database on integration).
install -m 644 "$LINUX_DIR/hyperpanes.desktop" "$APPDIR/usr/share/applications/hyperpanes.desktop"
install -m 644 "$LINUX_DIR/hyperpanes.desktop" "$APPDIR/hyperpanes.desktop"
install -m 644 "$LINUX_DIR/hyperpanes-mime.xml" "$APPDIR/usr/share/mime/packages/hyperpanes.xml"

# Icons: pre-derived hicolor PNGs (source: build/icon.png 512×512 — see
# rs/packaging/linux/README.md).
for png in "$LINUX_DIR"/icons/hicolor/*/apps/hyperpanes.png; do
  size_dir="$(basename "$(dirname "$(dirname "$png")")")"   # e.g. 256x256
  mkdir -p "$APPDIR/usr/share/icons/hicolor/$size_dir/apps"
  install -m 644 "$png" "$APPDIR/usr/share/icons/hicolor/$size_dir/apps/hyperpanes.png"
done
install -m 644 "$LINUX_DIR/icons/hicolor/512x512/apps/hyperpanes.png" "$APPDIR/hyperpanes.png"
install -m 644 "$LINUX_DIR/icons/hicolor/512x512/apps/hyperpanes.png" "$APPDIR/.DirIcon"

# AppRun: exec the real binary so std::env::current_exe() resolves to
# usr/bin/hyperpanes and the exe-relative resources/ lookup works.
cat > "$APPDIR/AppRun" <<'EOF'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
exec "$HERE/usr/bin/hyperpanes" "$@"
EOF
chmod 755 "$APPDIR/AppRun"

# Optional validation when the tooling is present (CI ubuntu images have it).
if command -v desktop-file-validate >/dev/null 2>&1; then
  echo "==> desktop-file-validate"
  desktop-file-validate "$APPDIR/hyperpanes.desktop"
fi
if command -v xmllint >/dev/null 2>&1; then
  echo "==> xmllint MIME xml"
  xmllint --noout "$APPDIR/usr/share/mime/packages/hyperpanes.xml"
fi

# --- 3. appimagetool (pinned), cached download --------------------------------
APPIMAGETOOL_VERSION="1.9.1"   # AppImage/appimagetool release tag (the old AppImageKit/13 assets are gone)
APPIMAGETOOL_URL="https://github.com/AppImage/appimagetool/releases/download/${APPIMAGETOOL_VERSION}/appimagetool-x86_64.AppImage"
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/hyperpanes-packaging"
TOOL="$CACHE_DIR/appimagetool-${APPIMAGETOOL_VERSION}-x86_64.AppImage"

if [ ! -x "$TOOL" ]; then
  echo "==> downloading appimagetool $APPIMAGETOOL_VERSION"
  mkdir -p "$CACHE_DIR"
  if command -v curl >/dev/null 2>&1; then
    curl -fL --retry 3 -o "$TOOL.tmp" "$APPIMAGETOOL_URL"
  elif command -v wget >/dev/null 2>&1; then
    wget -O "$TOOL.tmp" "$APPIMAGETOOL_URL"
  else
    err "need curl or wget to download appimagetool"
  fi
  chmod +x "$TOOL.tmp"
  mv "$TOOL.tmp" "$TOOL"
fi

# --- 4. emit the contract artifact --------------------------------------------
ARTIFACT="$OUT_DIR/hyperpanes-${VERSION}-${ARCH_TRIPLE}.AppImage"
echo "==> appimagetool → $ARTIFACT"
rm -f "$ARTIFACT"
# --appimage-extract-and-run: works without FUSE (containers, WSL, CI runners).
ARCH="$ARCH_TRIPLE" "$TOOL" --appimage-extract-and-run "$APPDIR" "$ARTIFACT"

[ -f "$ARTIFACT" ] || err "appimagetool reported success but $ARTIFACT is missing"
chmod +x "$ARTIFACT"
echo "==> done: $ARTIFACT"
