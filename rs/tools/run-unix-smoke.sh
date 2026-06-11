#!/usr/bin/env bash
# Launch an isolated instance and run unix-smoke.py against it. Usage: run-unix-smoke.sh [iso-dir]
set -u
ISO=${1:-/tmp/hp-smoke}
RS=$(cd "$(dirname "$0")/.." && pwd)
rm -rf "$ISO"
mkdir -p "$ISO/cfg/hyperpanes" "$ISO/state" "$ISO/data"
printf '%s' '{ "enabled": true, "allowInput": true }' > "$ISO/cfg/hyperpanes/control-settings.json"
env -u HYPERPANES_CONTROL_FILE -u HYPERPANES_PANE_ID \
  XDG_CONFIG_HOME=$ISO/cfg XDG_STATE_HOME=$ISO/state XDG_DATA_HOME=$ISO/data \
  HOME=${SMOKE_HOME:-$HOME} \
  nohup "$RS/crates/app/target/debug/hyperpanes" > "$ISO/app.log" 2>&1 &
echo $! > "$ISO/pid.txt"
sleep 8
# The second instance must resolve the SAME userData dir (the lock salt) as the primary.
env -u HYPERPANES_CONTROL_FILE -u HYPERPANES_PANE_ID \
  XDG_CONFIG_HOME=$ISO/cfg XDG_STATE_HOME=$ISO/state XDG_DATA_HOME=$ISO/data \
  SMOKE_BIN="$RS/crates/app/target/debug/hyperpanes" \
  python3 "$RS/tools/unix-smoke.py" "$ISO/state/hyperpanes/control.json" "$RS/crates/app/target/debug/hyperpanes"
rc=$?
kill "$(cat "$ISO/pid.txt")" 2>/dev/null
exit $rc
