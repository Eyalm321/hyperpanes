#!/usr/bin/env bash
# Reproduce (or verify the fix for) goal trcrash: with AccessKit active (an AT-SPI client
# listening), a Slint `focus()` run inside AccessKit's tree build panics the app with
# `RefCell already borrowed` in WinitWindowAdapter::handle_focus_change. Two shapes: a `changed`
# handler (the pane's `refocus-tick` after cancelling the New goal box) and an `init` focus of a
# dialog that AccessKit's build instantiates first (deterministic when the app boots straight into
# such a dialog via `--open <overlay>`).
#
# Fully isolated, safe to run from an agent pane:
#   * every hyperpanes dir lives in one scratch dir under ~/.cache: control file, XDG state/data/
#     config, HOME, TMPDIR (so the crash log lands there) and XDG_RUNTIME_DIR. Both app processes
#     start from `env -i`. The live app, its control.json and work.db are never touched; the live
#     control.json is hashed before and after, and the script fails if it changed;
#   * a private D-Bus session bus + its own AT-SPI bus (at-spi-bus-launcher --a11y=1, so
#     org.a11y.Status.IsEnabled = true), a hand-started at-spi2-registryd (AccessKit only activates
#     with a registry), and a pyatspi client that listens for focus events, as a screen reader does;
#   * a private display: headless sway (default; the user's crash was on Wayland) or Xvfb;
#   * the session daemon pre-started in the same env (otherwise the isolated GUI silently falls
#     back to in-process PTYs);
#   * the GUI and the daemon each run in their own transient systemd scope under
#     app-graphical.slice, never in the caller's memory-capped cgroup. Teardown stops them all.
#
# Steps: boot with HYPERPANES_OPEN=<--open> (the dialog opens at startup), require the app to be
# registered with the AT-SPI registry (else exit 2: the run would prove nothing), focus the
# dialog's text field through AT-SPI and press Escape (the dialog's cancel → State::close_overlay_now
# → refocus_active_pane_scope), then type a shell command. The marker file it writes proves the
# keyboard came back to the terminal.
#
# Usage: scripts/repro-a11y-focus-reentrancy.sh --bin PATH [--open <overlay>]  (default: newgoal)
#            [--display sway|xvfb] [--keep] [--out DIR] [--hold SECS]
# Exit: 0 = no panic, GUI alive, keyboard back in the terminal; 1 = panicked / died / no focus;
#       2 = setup error (including AccessKit never activating).
set -u

BIN=""
KEEP=0
OUT=""
DISPLAY_MODE=sway
HOLD=0
OPEN=newgoal
while [ $# -gt 0 ]; do
  case "$1" in
    --bin) BIN=$2; shift 2 ;;
    --keep) KEEP=1; shift ;;
    --out) OUT=$2; shift 2 ;;
    --display) DISPLAY_MODE=$2; shift 2 ;;
    --hold) HOLD=$2; shift 2 ;;
    --open) OPEN=$2; shift 2 ;;   # overlay to boot into (default newgoal)   # debugging: keep the env up N s after boot, before cancelling
    -h|--help) sed -n '2,32p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done
[ -x "$BIN" ] || { echo "--bin must point at a hyperpanes binary" >&2; exit 2; }
BIN=$(readlink -f "$BIN")
case "$DISPLAY_MODE" in
  sway) TOOLS="sway wtype wlrctl grim" ;;
  xvfb) TOOLS="Xvfb xdotool import" ;;
  *) echo "--display must be sway|xvfb" >&2; exit 2 ;;
esac
for t in dbus-daemon busctl python3 $TOOLS; do
  command -v "$t" >/dev/null || { echo "missing tool: $t" >&2; exit 2; }
done
LAUNCHER=/usr/libexec/at-spi-bus-launcher
[ -x "$LAUNCHER" ] || { echo "missing $LAUNCHER" >&2; exit 2; }

REAL_RUNTIME=/run/user/$(id -u)
LIVE_CTRL="$HOME/.local/state/hyperpanes/control.json"
LIVE_SUM_BEFORE=$(sha256sum "$LIVE_CTRL" 2>/dev/null | cut -d' ' -f1)

mkdir -p "$HOME/.cache"
D=$(mktemp -d -p "$HOME/.cache" hp-a11y.XXXXXX)
mkdir -p "$D"/{home,state,data,config,runtime,tmp}
chmod 700 "$D/runtime"
[ -n "$OUT" ] || OUT="$D/out"
mkdir -p "$OUT"
BUS="unix:path=$D/runtime/bus"
echo "scratch: $D"

PIDS_TO_KILL=()
UNITS=()
cleanup() {
  for u in "${UNITS[@]}"; do
    XDG_RUNTIME_DIR=$REAL_RUNTIME systemctl --user stop "$u" 2>/dev/null
  done
  for p in "${PIDS_TO_KILL[@]}"; do kill "$p" 2>/dev/null; done
  sleep 0.5
  for p in "${PIDS_TO_KILL[@]}"; do kill -9 "$p" 2>/dev/null; done
  [ -n "${DPY:-}" ] && rm -f "/tmp/.X${DPY#:}-lock" "/tmp/.X11-unix/X${DPY#:}"
  [ "$KEEP" = 1 ] || rm -rf "$D/home" "$D/data" "$D/runtime"
}
trap cleanup EXIT

# ---- private session bus + AT-SPI bus with accessibility ENABLED ----
XDG_RUNTIME_DIR=$D/runtime HOME=$D/home dbus-daemon --session --nofork --nopidfile \
  --address="$BUS" >"$D/dbus.log" 2>&1 &
PIDS_TO_KILL+=($!)
for _ in $(seq 50); do [ -S "$D/runtime/bus" ] && break; sleep 0.1; done
[ -S "$D/runtime/bus" ] || { echo "private session bus did not come up" >&2; exit 2; }

env -u DISPLAY -u WAYLAND_DISPLAY XDG_RUNTIME_DIR="$D/runtime" HOME="$D/home" \
  DBUS_SESSION_BUS_ADDRESS="$BUS" GSETTINGS_BACKEND=memory \
  "$LAUNCHER" --launch-immediately --a11y=1 >"$D/atspi-launcher.log" 2>&1 &
PIDS_TO_KILL+=($!)
EN=""
for _ in $(seq 50); do
  EN=$(busctl --address="$BUS" get-property org.a11y.Bus /org/a11y/bus org.a11y.Status IsEnabled 2>/dev/null)
  [ "$EN" = "b true" ] && break
  sleep 0.1
done
A11Y=$(busctl --address="$BUS" call org.a11y.Bus /org/a11y/bus org.a11y.Bus GetAddress 2>/dev/null |
  sed -E 's/^s "(.*)"$/\1/')
echo "a11y bus: ${A11Y:-<none>}  IsEnabled: ${EN:-?}"
[ "$EN" = "b true" ] && [ -n "$A11Y" ] || { echo "AT-SPI bus not enabled" >&2; exit 2; }
# The registry daemon. The a11y bus (dbus-broker under the launcher) would activate it through
# the systemd user manager, which fails for a private bus ("unit failed"), and without a registry
# AccessKit's Embed call fails and it never activates. So start it here, in the isolated env.
env -i HOME="$D/home" XDG_RUNTIME_DIR="$D/runtime" DBUS_SESSION_BUS_ADDRESS="$BUS" PATH=/usr/bin:/bin \
  /usr/libexec/at-spi2-registryd >"$D/registryd.log" 2>&1 &
PIDS_TO_KILL+=($!)
for _ in $(seq 50); do
  busctl --address="$A11Y" --no-pager list 2>/dev/null | grep -q '^org.a11y.atspi.Registry .*registr' && break
  sleep 0.1
done
busctl --address="$A11Y" call org.a11y.atspi.Registry /org/a11y/atspi/accessible/root \
  org.a11y.atspi.Accessible GetChildren >/dev/null 2>&1 || { echo "AT-SPI registry not up" >&2; exit 2; }

# ---- private display: a headless Wayland compositor (default) or Xvfb ----
# Wayland matters: the user's crash trace is from the Wayland backend, where rendering waits for
# the compositor's frame callback. That lets a timer pass run "pump pushes the closed overlay +
# refocus tick, then AccessKit's reload_tree builds the tree" before the next render, so the
# change handler runs inside the AccessKit borrow. Under Xvfb the render follows the pump at once
# and runs the handler safely first (see the Xvfb control run).
DPY=""
WL=""
case "$DISPLAY_MODE" in
  xvfb)
    for n in $(seq 140 199); do
      [ -e "/tmp/.X11-unix/X$n" ] || [ -e "/tmp/.X$n-lock" ] || { DPY=":$n"; break; }
    done
    Xvfb "$DPY" -screen 0 1280x800x24 -nolisten tcp >"$D/xvfb.log" 2>&1 &
    PIDS_TO_KILL+=($!)
    for _ in $(seq 50); do [ -e "/tmp/.X11-unix/X${DPY#:}" ] && break; sleep 0.1; done
    DISPLAY_ENV=("DISPLAY=$DPY")
    echo "display: Xvfb $DPY" ;;
  sway)
    printf 'output HEADLESS-1 resolution 1280x800\nxwayland disable\ndefault_border none\n' >"$D/sway.conf"
    env -i HOME="$D/home" XDG_RUNTIME_DIR="$D/runtime" PATH=/usr/bin:/bin \
      WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 sway -c "$D/sway.conf" >"$D/sway.log" 2>&1 &
    PIDS_TO_KILL+=($!)
    for _ in $(seq 100); do
      WL=$(cd "$D/runtime" && ls wayland-* 2>/dev/null | grep -v '\.lock$' | head -1)
      [ -n "$WL" ] && break
      sleep 0.1
    done
    [ -n "$WL" ] || { echo "headless sway did not come up"; tail -5 "$D/sway.log"; exit 2; }
    DISPLAY_ENV=("WAYLAND_DISPLAY=$WL")
    echo "display: headless sway, $D/runtime/$WL" ;;
esac
wl() { env XDG_RUNTIME_DIR="$D/runtime" WAYLAND_DISPLAY="$WL" "$@"; }
shot() {
  case "$DISPLAY_MODE" in
    xvfb) DISPLAY=$DPY import -window root "$1" 2>/dev/null ;;
    sway) wl grim "$1" 2>/dev/null ;;
  esac
}
# Each wtype run adds a fresh virtual keyboard, and its FIRST key is dropped (it races the
# client's keymap switch). So every sway invocation leads with a pause + a throwaway Shift tap.
press_escape() {
  case "$DISPLAY_MODE" in
    xvfb) DISPLAY=$DPY xdotool key Escape ;;
    sway) wl wtype -s 300 -k Shift_L -s 150 -k Escape ;;
  esac
}
type_line() {
  case "$DISPLAY_MODE" in
    xvfb) DISPLAY=$DPY xdotool type --delay 40 "$1"; DISPLAY=$DPY xdotool key Return ;;
    sway) wl wtype -s 300 -k Shift_L -s 150 -d 40 "$1" -k Return ;;
  esac
}

ENVV=(
  "HOME=$D/home"
  "HYPERPANES_CONTROL_FILE=$D/state/hyperpanes/control.json"
  "XDG_STATE_HOME=$D/state" "XDG_DATA_HOME=$D/data" "XDG_CONFIG_HOME=$D/config"
  "XDG_RUNTIME_DIR=$D/runtime" "TMPDIR=$D/tmp"
  "DBUS_SESSION_BUS_ADDRESS=$BUS" "GSETTINGS_BACKEND=memory"
  "${DISPLAY_ENV[@]}" "HYPERPANES_A11Y=1" "RUST_BACKTRACE=1" "HYPERPANES_DEBUG=1"
  "SHELL=/bin/bash" "PATH=/usr/local/bin:/usr/bin:/bin" "LANG=C.UTF-8" "TERM=xterm-256color"
)
# Both processes start from `env -i` + this list: nothing leaks in from the calling pane (its
# HYPERPANES_PANE_ID / control token / live control file, WAYLAND_DISPLAY, a stale XAUTHORITY).

# ---- session daemon, pre-started in the isolated env (own scope) ----
TAG="hp-a11y-$$-$RANDOM"
systemd-run --user --scope --quiet --slice=app-graphical.slice --unit="$TAG-daemon" \
  env -i "${ENVV[@]}" "$BIN" --session-daemon "$D/config/hyperpanes" \
  >"$D/daemon.out" 2>"$D/daemon.err" &
UNITS+=("$TAG-daemon.scope")
sleep 1.5

# ---- AT client: listen for focus events like a screen reader (on the PRIVATE a11y stack) ----
cat >"$D/at-client.py" <<'PY'
import sys, pyatspi
log = open(sys.argv[1], "a", buffering=1)
def on_focus(ev):
    try:
        log.write(f"focus: {ev.source.getRoleName()} {ev.source.name!r} app={ev.host_application.name!r}\n")
    except Exception as e:
        log.write(f"focus: <{e}>\n")
pyatspi.Registry.registerEventListener(on_focus, "object:state-changed:focused")
log.write("at-client: listening\n")
pyatspi.Registry.start()
PY
env -i HOME="$D/home" XDG_RUNTIME_DIR="$D/runtime" DBUS_SESSION_BUS_ADDRESS="$BUS" \
  GSETTINGS_BACKEND=memory PATH=/usr/bin:/bin \
  python3 -I "$D/at-client.py" "$OUT/at-client.log" >"$D/at-client.err" 2>&1 &
PIDS_TO_KILL+=($!)

# ---- the GUI, New goal box open at startup ----
systemd-run --user --scope --quiet --slice=app-graphical.slice --unit="$TAG-gui" \
  env -i "${ENVV[@]}" "HYPERPANES_OPEN=$OPEN" "$BIN" \
  >"$D/gui.out" 2>"$D/gui.err" &
UNITS+=("$TAG-gui.scope")

gui_pid() {
  local cg
  cg=$(XDG_RUNTIME_DIR=$REAL_RUNTIME systemctl --user show -p ControlGroup --value "$TAG-gui.scope" 2>/dev/null)
  case "$cg" in ""|/) return ;; esac
  [ -r "/sys/fs/cgroup$cg/cgroup.procs" ] || return
  for p in $(cat "/sys/fs/cgroup$cg/cgroup.procs"); do
    [ "$(readlink -f /proc/$p/exe 2>/dev/null)" = "$BIN" ] &&
      ! tr '\0' ' ' </proc/$p/cmdline | grep -q -- '--crash-report' && { echo "$p"; return; }
  done
}

CRASH="$D/tmp/hyperpanes-crash.log"
panic_summary() {
  local n=0
  [ -f "$CRASH" ] && n=$(grep -c '^PANIC:' "$CRASH")
  echo "panic records:         $n"
  if [ "$n" -gt 0 ]; then
    cp "$CRASH" "$OUT/hyperpanes-crash.log"
    grep -m1 -A1 '^PANIC:' "$CRASH" | sed 's/^/  /'
    grep -m1 'handle_focus_change' "$CRASH" | sed 's/^/  frame: /'
    grep -m1 -E '::init$|ChangeTracker' "$CRASH" | sed 's/^/  frame: /'
    grep -m1 'build_new_tree' "$CRASH" | sed 's/^/  frame: /'
    grep -m1 -E 'process_accesskit_event|reload_tree' "$CRASH" | sed 's/^/  frame: /'
  fi
  return 0
}

WID=""
if [ "$DISPLAY_MODE" = xvfb ]; then
  for _ in $(seq 120); do
    WID=$(DISPLAY=$DPY xdotool search --onlyvisible --name hyperpanes 2>/dev/null | head -1)
    [ -n "$WID" ] && break
    sleep 0.25
  done
  [ -n "$WID" ] || { echo "GUI window never appeared"; tail -20 "$D/gui.err"; exit 1; }
  # No window manager on Xvfb: give the window X keyboard focus, as a desktop would.
  DISPLAY=$DPY xdotool windowfocus --sync "$WID" 2>/dev/null
fi
sleep 8   # boot settle: first frames, AccessKit activation + initial tree, the box opens
GPID=$(gui_pid)
echo "gui pid: ${GPID:-<dead>}"
if [ -z "$GPID" ]; then
  echo "GUI died during boot"
  panic_summary
  echo "RESULT: FAIL"
  exit 1
fi

# Is the app on the AT-SPI bus, i.e. is AccessKit really active? (Otherwise the run is vacuous.)
NAMES=$(busctl --address="$A11Y" --no-pager list 2>/dev/null | awk -v p="$GPID" '$2==p {print $1}')
REG=$(busctl --address="$A11Y" call org.a11y.atspi.Registry /org/a11y/atspi/accessible/root \
  org.a11y.atspi.Accessible GetChildren 2>/dev/null)
REGISTERED=0
for n in $NAMES; do printf '%s' "$REG" | grep -q "\"$n\"" && REGISTERED=1; done
echo "AccessKit active (app registered with the AT-SPI registry): $([ $REGISTERED = 1 ] && echo yes || echo NO)"
if [ "$REGISTERED" != 1 ]; then
  echo "--- a11y bus connections (name pid process) ---"
  busctl --address="$A11Y" --no-pager list 2>&1 | awk '{print "  " $1, $2, $3}' | head -20
  echo "--- registry children: ${REG:-<no reply>}"
fi
[ "$REGISTERED" = 1 ] || { echo "AccessKit never activated: the run would prove nothing" >&2; exit 2; }

shot "$OUT/1-newgoal-open.png"
if [ "$HOLD" -gt 0 ]; then
  echo "holding ${HOLD}s: XDG_RUNTIME_DIR=$D/runtime WAYLAND_DISPLAY=$WL DISPLAY=$DPY"
  sleep "$HOLD"
fi

# ---- Cancel (the user's Cancel → State::close_overlay_now → refocus_active_pane_scope) ----
# Put the keyboard in the dialog's text field the way a user's click would, through the AT-SPI
# client (Component.grabFocus → AccessKit SetFocus; a freshly opened box has no Slint focus
# item yet), then press Escape, which the field forwards as the dialog's cancel.
cat >"$D/at-focus-entry.py" <<'PY'
import pyatspi
def find(n, d=0):
    if n is None or d > 25: return None
    if n.getRoleName() == "entry": return n
    for c in n:
        r = find(c, d + 1)
        if r: return r
for app in pyatspi.Registry.getDesktop(0):
    if app is not None and app.name.startswith("hyperpanes"):
        e = find(app)
        print("AT-SPI grabFocus(entry):", e.queryComponent().grabFocus() if e else "no entry")
PY
env -i HOME="$D/home" XDG_RUNTIME_DIR="$D/runtime" DBUS_SESSION_BUS_ADDRESS="$BUS" \
  GSETTINGS_BACKEND=memory PATH=/usr/bin:/bin timeout 20 python3 -I "$D/at-focus-entry.py"
sleep 0.5
echo "cancelling the $OPEN dialog: Escape"
press_escape
sleep 3
shot "$OUT/2-after-cancel.png"

# ---- keyboard back in the terminal? type a command that writes a marker ----
MARK="$D/marker-typed-into-terminal"
if [ -n "$(gui_pid)" ]; then
  type_line "echo trcrash-focus-ok > $MARK"
  sleep 2
  shot "$OUT/3-typed.png"
fi

PANICS=0
[ -f "$CRASH" ] && PANICS=$(grep -c '^PANIC:' "$CRASH")
GPID_AFTER=$(gui_pid)
FOCUS_OK=0
[ -f "$MARK" ] && grep -q trcrash-focus-ok "$MARK" && FOCUS_OK=1

panic_summary
echo "gui alive after cancel: $([ -n "$GPID_AFTER" ] && echo "yes (pid $GPID_AFTER)" || echo no)"
echo "keyboard back in terminal (typed marker written): $([ $FOCUS_OK = 1 ] && echo yes || echo no)"
echo "AT client saw focus events: $(grep -c '^focus:' "$OUT/at-client.log" 2>/dev/null || echo 0)"
cp "$D/gui.err" "$OUT/gui.err" 2>/dev/null

LIVE_SUM_AFTER=$(sha256sum "$LIVE_CTRL" 2>/dev/null | cut -d' ' -f1)
if [ "$LIVE_SUM_BEFORE" != "$LIVE_SUM_AFTER" ]; then
  echo "LIVE control.json CHANGED during the run — isolation breach!" >&2
  exit 2
fi
echo "live control.json untouched (sha256 ${LIVE_SUM_AFTER:0:12}…)"
echo "evidence: $OUT"

if [ "$PANICS" -gt 0 ] || [ -z "$GPID_AFTER" ] || [ "$FOCUS_OK" = 0 ]; then
  echo "RESULT: FAIL"
  exit 1
fi
echo "RESULT: OK"
exit 0
