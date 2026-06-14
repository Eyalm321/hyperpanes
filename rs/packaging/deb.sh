#!/usr/bin/env bash
# Build the hyperpanes .deb for x86_64 Linux via cargo-deb.
#
# Contract (mirrors appimage.sh):
#   rs/packaging/deb.sh <version>     # <version> WITHOUT a leading "v"
#   → rs/packaging/out/hyperpanes_<version>_amd64.deb
# Runs from any cwd; exits non-zero on any failure; artifact under rs/packaging/out/.
#
# Layout (FHS): /usr/bin/hyperpanes + /usr/share/hyperpanes/resources/shell-integration/…
# (the assets list lives in crates/app/Cargo.toml [package.metadata.deb]). No root needed —
# cargo-deb is a pure-Rust cargo plugin, installed on demand if absent.

set -euo pipefail

err() { echo "deb.sh: error: $*" >&2; exit 1; }

VERSION="${1:-}"
[ -n "$VERSION" ] || err "usage: deb.sh <version>  (e.g. 0.0.8, no leading 'v')"
case "$VERSION" in v*) err "<version> must not have a leading 'v' (got '$VERSION')";; esac
[ "$(uname -m)" = "x86_64" ] || err "this script targets x86_64 (host is $(uname -m))"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
APP_MANIFEST="$ROOT/rs/crates/app/Cargo.toml"
OUT_DIR="$SCRIPT_DIR/out"
[ -f "$APP_MANIFEST" ] || err "app manifest not found at $APP_MANIFEST"
mkdir -p "$OUT_DIR"

command -v cargo >/dev/null 2>&1 || err "cargo not found on PATH"

# The artifact is named from <version>; keep the embedded crate version in lockstep.
MANIFEST_VER="$(sed -n 's/^version = "\([^"]*\)".*/\1/p' "$APP_MANIFEST" | head -1)"
[ "$MANIFEST_VER" = "$VERSION" ] || \
  err "crates/app/Cargo.toml version ($MANIFEST_VER) != requested ($VERSION); bump it first"

if ! cargo deb --version >/dev/null 2>&1; then
  echo "==> installing cargo-deb (one-time)"
  cargo install cargo-deb
fi

ARTIFACT="$OUT_DIR/hyperpanes_${VERSION}_amd64.deb"
echo "==> cargo deb → $ARTIFACT"
rm -f "$ARTIFACT"
# cargo-deb runs its own --release build; assets reference target/release/hyperpanes.
cargo deb --manifest-path "$APP_MANIFEST" --output "$ARTIFACT"

[ -f "$ARTIFACT" ] || err "cargo deb reported success but $ARTIFACT is missing"
echo "==> done: $ARTIFACT"
