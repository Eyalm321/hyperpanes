#!/usr/bin/env bash
# macOS: launch an isolated instance (temp HOME) + run the parity probe and unix smoke.
# Usage: run-mac-smoke.sh [iso-dir]
set -u
ISO=${1:-/tmp/hp-smoke-mac}
RS=$(cd "$(dirname "$0")/.." && pwd)
BIN="$RS/crates/app/target/debug/hyperpanes"
APPDIR="$ISO/home/Library/Application Support/hyperpanes"
rm -rf "$ISO"
mkdir -p "$APPDIR"
printf '%s' '{ "enabled": true, "allowInput": true }' > "$APPDIR/control-settings.json"
run() { env -u HYPERPANES_CONTROL_FILE -u HYPERPANES_PANE_ID HOME="$ISO/home" "$@"; }
run nohup "$BIN" > "$ISO/app.log" 2>&1 &
echo $! > "$ISO/pid.txt"
sleep 10
CJ="$APPDIR/control.json"
if [ ! -f "$CJ" ]; then
  echo "NO control.json — app failed to start:"; tail -10 "$ISO/app.log"
  tail -5 "${TMPDIR:-/tmp}/hyperpanes-crash.log" 2>/dev/null
  exit 1
fi
echo "=== parity probe ==="
run python3 "$RS/tools/control-parity-probe.py" "$CJ" "$ISO/shapes-macos.json"
echo "=== unix smoke ==="
run python3 "$RS/tools/unix-smoke.py" "$CJ" "$BIN"
rc=$?
kill "$(cat "$ISO/pid.txt")" 2>/dev/null
exit $rc
