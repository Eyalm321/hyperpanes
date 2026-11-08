#!/usr/bin/env bash
# Boot an isolated hyperpanes GUI with (or without) a D-Bus session bus and report whether it
# panicked. Reproduces the zbus "there is no reactor running" crash (fix/zbus-no-reactor) and
# verifies the fix. Safe to run from an agent pane:
#   * every hyperpanes dir is diverted into one scratch dir under ~/.cache (control file, state,
#     data, config, runtime, TMPDIR — so the crash log + marker land there too), so it never
#     touches the live control plane, work queue or crash log;
#   * the GUI runs on a private Xvfb display by default (`--display host` uses your real X/Wayland
#     display instead), inside its own transient systemd scope under app-graphical.slice, never
#     in the caller's (memory-capped) cgroup;
#   * the scope is stopped at the end, which also kills any crash-report dialog the panic hook
#     spawned (it is never clicked, so nothing is sent anywhere).
#
# Usage: scripts/repro-no-session-bus.sh [--bin PATH] [--bus none|session] [--display xvfb|host]
#                                        [--secs N] [--then CMD] [--keep]
#   --bus none     (default) unset DBUS_SESSION_BUS_ADDRESS and point XDG_RUNTIME_DIR at an
#                  empty dir: zbus then dials $XDG_RUNTIME_DIR/bus, which does not exist.
#   --bus session  keep the real session bus (the "feature still works" control run).
#   --then CMD     run CMD (via sh -c, with $GUI_PID set, and DISPLAY set to the Xvfb display in
#                  xvfb mode) while the GUI is still up, before teardown; e.g. scripts/atspi-check.sh to list its D-Bus/AT-SPI presence.
#
# Exit: 0 = no panic and the GUI is still running after --secs; 1 = panicked (crash log has a
# PANIC record) or the GUI died; 2 = usage/setup error.
set -u

BIN=""
BUS=none
DISPLAY_MODE=xvfb
SECS=12
KEEP=0
THEN=""
while [ $# -gt 0 ]; do
  case "$1" in
    --bin) BIN=$2; shift 2 ;;
    --bus) BUS=$2; shift 2 ;;
    --display) DISPLAY_MODE=$2; shift 2 ;;
    --secs) SECS=$2; shift 2 ;;
    --then) THEN=$2; shift 2 ;;
    --keep) KEEP=1; shift ;;
    -h|--help) sed -n '2,29p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done
REPO=$(cd "$(dirname "$0")/.." && pwd)
[ -n "$BIN" ] || BIN="$REPO/rs/crates/app/target/debug/hyperpanes"
[ -x "$BIN" ] || { echo "no hyperpanes binary at $BIN (build rs/crates/app or pass --bin)" >&2; exit 2; }

mkdir -p "$HOME/.cache"
D=$(mktemp -d -p "$HOME/.cache" hp-nobus.XXXXXX)
mkdir -p "$D"/{home,state,data,config,runtime,tmp}
chmod 700 "$D/runtime"

ENVV=(
  "HOME=$D/home"
  "HYPERPANES_CONTROL_FILE=$D/state/hyperpanes/control.json"
  "XDG_STATE_HOME=$D/state" "XDG_DATA_HOME=$D/data" "XDG_CONFIG_HOME=$D/config"
  "XDG_RUNTIME_DIR=$D/runtime" "TMPDIR=$D/tmp"
  "HYPERPANES_DEBUG=1" "RUST_BACKTRACE=1"
)
UNSET=(-u WAYLAND_DISPLAY -u HYPERPANES_PANE_ID -u HYPERPANES_CONTROL_TOKEN -u HYPERPANES_CONTROL_PORT)
case "$BUS" in
  none) UNSET+=(-u DBUS_SESSION_BUS_ADDRESS) ;;
  session)
    : "${DBUS_SESSION_BUS_ADDRESS:=unix:path=/run/user/$(id -u)/bus}"
    ENVV+=("DBUS_SESSION_BUS_ADDRESS=$DBUS_SESSION_BUS_ADDRESS") ;;
  *) echo "--bus must be none|session" >&2; exit 2 ;;
esac

XVFB_PID=""
case "$DISPLAY_MODE" in
  xvfb)
    command -v Xvfb >/dev/null || { echo "Xvfb not installed (use --display host)" >&2; exit 2; }
    for n in $(seq 140 199); do [ -e "/tmp/.X11-unix/X$n" ] || [ -e "/tmp/.X$n-lock" ] || { DPY=":$n"; break; }; done
    Xvfb "$DPY" -screen 0 1600x1000x24 -nolisten tcp >"$D/xvfb.log" 2>&1 &
    XVFB_PID=$!
    for _ in $(seq 50); do [ -e "/tmp/.X11-unix/X${DPY#:}" ] && break; sleep 0.1; done
    ENVV+=("DISPLAY=$DPY") ;;
  host)
    # Prefer the Wayland socket by absolute path (XDG_RUNTIME_DIR is diverted above, so a bare
    # "wayland-0" would not resolve); fall back to X11.
    WL="${WAYLAND_DISPLAY:-}"
    case "$WL" in /*) ;; ?*) WL="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/$WL" ;; esac
    if [ -n "$WL" ] && [ -S "$WL" ]; then
      ENVV+=("WAYLAND_DISPLAY=$WL")
    elif [ -n "${DISPLAY:-}" ]; then
      ENVV+=("DISPLAY=$DISPLAY")
    else
      echo "--display host needs \$WAYLAND_DISPLAY or \$DISPLAY" >&2; exit 2
    fi ;;
  *) echo "--display must be xvfb|host" >&2; exit 2 ;;
esac

UNIT="hp-nobus-$$-$RANDOM"
echo "scratch:  $D"
echo "unit:     $UNIT.scope (app-graphical.slice)"
echo "command:  env ${UNSET[*]} ${ENVV[*]} $BIN"
systemd-run --user --scope --quiet --slice=app-graphical.slice --unit="$UNIT" \
  env "${UNSET[@]}" "${ENVV[@]}" "$BIN" >"$D/stdout.log" 2>"$D/stderr.log" &
RUN_PID=$!

sleep "$SECS"

CRASH="$D/tmp/hyperpanes-crash.log"
ALIVE=0
# The GUI is the scope's main process; the crash reporter (if any) is a `--crash-report` child.
PIDS=$(systemctl --user show -p ControlGroup --value "$UNIT.scope" 2>/dev/null)
GUI_PID=""
if [ -n "$PIDS" ] && [ -r "/sys/fs/cgroup$PIDS/cgroup.procs" ]; then
  for p in $(cat "/sys/fs/cgroup$PIDS/cgroup.procs"); do
    cmd=$(tr '\0' ' ' 2>/dev/null <"/proc/$p/cmdline")
    case "$cmd" in
      "$BIN "|"$BIN") GUI_PID=$p ;;
    esac
    case "$cmd" in *--crash-report*) echo "crash-reporter child running: pid $p (will be killed)";; esac
  done
fi
[ -n "$GUI_PID" ] && ALIVE=1

PANICS=0
[ -f "$CRASH" ] && PANICS=$(grep -c '^PANIC:' "$CRASH")
echo "gui alive after ${SECS}s: $([ $ALIVE = 1 ] && echo "yes (pid $GUI_PID)" || echo no)"
echo "panic records:           $PANICS"
if [ "$PANICS" -gt 0 ]; then
  echo "--- first panic ---"
  grep -m1 -A2 '^PANIC:' "$CRASH"
  grep -m1 -E 'accesskit_unix|ksni|xdg_desktop_settings|ashpd|rfd::' "$CRASH" | sed 's/^/  frame: /'
fi
echo "--- D-Bus lines on stderr ---"
grep -i -E 'd-bus|dbus|session bus|accessib' "$D/stderr.log" | head -5
if [ -n "$THEN" ] && [ -n "$GUI_PID" ]; then
  echo "--- --then: $THEN ---"
  if [ -n "${DPY:-}" ]; then
    GUI_PID=$GUI_PID DISPLAY=$DPY sh -c "$THEN"
  else
    GUI_PID=$GUI_PID sh -c "$THEN"
  fi
fi

systemctl --user stop "$UNIT.scope" 2>/dev/null
wait "$RUN_PID" 2>/dev/null
[ -n "$XVFB_PID" ] && kill "$XVFB_PID" 2>/dev/null
[ "$KEEP" = 1 ] || rm -rf "$D"
[ "$KEEP" = 1 ] && echo "kept:     $D"

if [ "$PANICS" -gt 0 ] || [ "$ALIVE" = 0 ]; then
  echo "RESULT: FAIL (panicked or died)"
  exit 1
fi
echo "RESULT: OK (no panic, still running)"
exit 0
