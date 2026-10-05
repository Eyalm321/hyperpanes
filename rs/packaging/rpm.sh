#!/usr/bin/env bash
# Build the hyperpanes .rpm for x86_64 Linux via cargo-generate-rpm.
#
# Contract (mirrors appimage.sh):
#   rs/packaging/rpm.sh <version>     # <version> WITHOUT a leading "v"
#   → rs/packaging/out/hyperpanes-<version>-1.x86_64.rpm
# Runs from any cwd; exits non-zero on any failure; artifact under rs/packaging/out/.
#
# Layout (FHS): /usr/bin/hyperpanes + /usr/share/hyperpanes/resources/shell-integration/…
# (the assets list lives in crates/app/Cargo.toml [package.metadata.generate-rpm]). No root and
# no rpm-build tooling needed — cargo-generate-rpm is a pure-Rust cargo plugin that writes the
# rpm header + cpio payload directly; installed on demand if absent.

set -euo pipefail

err() { echo "rpm.sh: error: $*" >&2; exit 1; }

VERSION="${1:-}"
[ -n "$VERSION" ] || err "usage: rpm.sh <version>  (e.g. 0.0.8, no leading 'v')"
case "$VERSION" in v*) err "<version> must not have a leading 'v' (got '$VERSION')";; esac
[ "$(uname -m)" = "x86_64" ] || err "this script targets x86_64 (host is $(uname -m))"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
APP_DIR="$ROOT/rs/crates/app"
APP_MANIFEST="$APP_DIR/Cargo.toml"
OUT_DIR="$SCRIPT_DIR/out"
[ -f "$APP_MANIFEST" ] || err "app manifest not found at $APP_MANIFEST"
mkdir -p "$OUT_DIR"

command -v cargo >/dev/null 2>&1 || err "cargo not found on PATH"

MANIFEST_VER="$(sed -n 's/^version = "\([^"]*\)".*/\1/p' "$APP_MANIFEST" | head -1)"
[ "$MANIFEST_VER" = "$VERSION" ] || \
  err "crates/app/Cargo.toml version ($MANIFEST_VER) != requested ($VERSION); bump it first"

if ! cargo generate-rpm --version >/dev/null 2>&1; then
  echo "==> installing cargo-generate-rpm (one-time)"
  cargo install cargo-generate-rpm
fi

# cargo-generate-rpm packages an already-built binary — build release first.
echo "==> cargo build --release (rs/crates/app)"
cargo build --release --manifest-path "$APP_MANIFEST"

ARTIFACT="$OUT_DIR/hyperpanes-${VERSION}-1.x86_64.rpm"
echo "==> cargo generate-rpm → $ARTIFACT"
rm -f "$ARTIFACT"
# Run from the crate dir: cargo-generate-rpm reads ./Cargo.toml, takes target/release and the
# relative asset `source` paths from the cwd, and its `-p` flag is a workspace MEMBER NAME (not
# a path) — so cd in instead. `-o` is absolute, unaffected by the cwd change.
( cd "$APP_DIR" && cargo generate-rpm -o "$ARTIFACT" )

[ -f "$ARTIFACT" ] || err "cargo generate-rpm reported success but $ARTIFACT is missing"
echo "==> done: $ARTIFACT"
