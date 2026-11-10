#!/usr/bin/env bash
# Isolated GUI E2E for multi-window restore / keep-alive quit / close-window confirm.
# Usage: e2e.sh [binary]   (default $CARGO_TARGET_DIR/debug/hyperpanes). Needs Xvfb, xdotool, import, jq, systemd --user.
# Everything lives under $EVD/run/t; the live app, its control.json, daemon and state are never touched.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
EVD=${EVD:-${XDG_CACHE_HOME:-$HOME/.cache}/hyperpanes/e2e-multiwindow}   # evidence: run dir, shots, logs
mkdir -p "$EVD"
BIN=${1:-${CARGO_TARGET_DIR:?}/debug/hyperpanes}
source "$HERE/lib.sh"
BIN=${1:-$BIN}
RES=$EVD/results.txt; : >"$RES"
rm -f "$EVD"/shots/*.png; mkdir -p "$EVD/shots"
shot() { import -display "$DPY" -window root "$EVD/shots/$1.png" 2>/dev/null; }
rec() { # PASS|FAIL name evidence
  echo "$1 | $2 | $3" | tee -a "$RES"; }
check() { # name expected actual
  if [ "$2" = "$3" ]; then rec PASS "$1" "$3"; else rec FAIL "$1" "expected [$2] got [$3]"; fi; }
fake_pid() { pgrep -f '^fake-claude 100000' | head -1; }
daemon_pid() { pgrep -f "^$BIN --session-daemon $T/config/hyperpanes" | head -1; }
alive() { [ -n "${1:-}" ] && kill -0 "$1" 2>/dev/null && echo alive || echo dead; }
xwins() { DISPLAY=$DPY xdotool search --onlyvisible --name . 2>/dev/null | sort -n; }
xw() { xwins | sed -n "$(( $1 + 1 ))p"; }     # n-th X window (0-based) in creation order
wpos() { DISPLAY=$DPY xdotool getwindowgeometry --shell "$1" 2>/dev/null | sed -n 's/^\(X\|Y\)=//p' | tr '\n' ' '; }
xw_at() { local w; for w in $(xwins); do [ "$(wpos "$w")" = "$1 $2 " ] && { echo "$w"; return; }; done; }
xwa() { local w; w=$(xw_at 0 0); echo "${w:-$(xw 0)}"; }        # window at the origin (windowId 0)
xwb() { local w; w=$(xw_at 176 126); echo "${w:-$(xw 1)}"; }    # the cascaded second window
click() { local o; DISPLAY=$DPY xdotool windowraise "$1"; o=($(wpos "$1")); DISPLAY=$DPY xdotool mousemove $(( ${o[0]:-0} + $2 )) $(( ${o[1]:-0} + $3 )) click 1; sleep 0.6; }
body() { click "$1" 300 400; }                  # a real click inside the window (winit drops keys before one)
key() { DISPLAY=$DPY xdotool windowfocus "$1"; sleep 0.2; DISPLAY=$DPY xdotool key --window "$1" "$2"; sleep 0.8; }
click_x() { click "$1" 1258 16; sleep 0.8; }
# Optional (E2E_NUDGE=1): a 1px resize after each relaunch, standing in for a WM configure. Not needed since
# restored windows apply their saved size after show(); kept to tell a layout regression from a harness issue.
nudge() { [ -z "${E2E_NUDGE:-}" ] && { sleep 1.5; return; }; local w; for w in $(xwins); do DISPLAY=$DPY xdotool windowsize "$w" 1281 801; done; sleep 1.5; }
nwin() { st | jq '.windows|length'; }
gui_gone() { for _ in $(seq 60); do systemctl --user is-active "$GUI_UNIT.scope" >/dev/null 2>&1 || { echo gone; return; }; sleep 0.5; done; echo still-running; }
wait_windows() { for _ in $(seq 100); do [ "$(nwin 2>/dev/null)" = "$1" ] && return 0; sleep 0.3; done; return 1; }
new_window_via_palette() { # $1 = x window to drive
  body "$1"; key "$1" ctrl+shift+p; DISPLAY=$DPY xdotool windowfocus "$1"; DISPLAY=$DPY xdotool type --window "$1" --delay 80 "New window"; sleep 0.6
  key "$1" Return; sleep 1.5; }
fake_claude_pane() { st | jq -c '[.windows[]|{w:.windowId} as $w|.tabs[].panes[]|select(.label=="fake-claude")|{windowId:$w.w,id,meta}]|.[0]'; }
snap_panes() { st | jq -c '[.windows[]|{windowId,tabs:[.tabs[]|{title,panes:[.panes[].label]}]}]'; }

# assert_restored <tag> <nwindows> <pid-before> : post-relaunch checks from /state, ps, last-workspace.json
assert_restored() {
  local tag=$1 n=$2 pid=$3 snap=$T/state/hyperpanes/last-workspace.json
  wait_windows "$n"
  check "$tag windows in /state" "$n" "$(nwin)"
  rec INFO "$tag layout" "$(snap_panes)"
  local fp; fp=$(fake_claude_pane)
  rec INFO "$tag fake-claude pane" "$fp"
  check "$tag fake-claude pid unchanged" "$pid" "$(fake_pid)"
  check "$tag meta role" '"lead"' "$(echo "$fp" | jq -c .meta.role)"
  check "$tag meta parent" '"p-x"' "$(echo "$fp" | jq -c .meta.parent)"
  check "$tag meta team" '"t1"' "$(echo "$fp" | jq -c .meta.team)"
  check "$tag someState absent" null "$(echo "$fp" | jq -c .meta.someState)"
  check "$tag hp.recovered absent" null "$(echo "$fp" | jq -c '.meta["hp.recovered"]')"
  sleep 6   # let autosave rewrite the snapshot from the restored state
  check "$tag last-workspace.json windows" "$n" "$(jq '.windows|length' "$snap")"
  rec INFO "$tag last-workspace fake-claude spec" "$(jq -c '[.windows[].groups[].panes[]|select(.label=="fake-claude")|{command,args,meta}]' "$snap")"
  shot "$tag-restored"
}
quit_gui_state() { # $1 tag ; after the GUI exited: daemon + pid still alive
  check "$1 GUI exited (scope gone)" gone "$(gui_gone)"
  check "$1 daemon alive" alive "$(alive "$(daemon_pid)")"
  check "$1 fake-claude alive" alive "$(alive "$(fake_pid)")"
}
cleanup() {
  [ "${KEEP:-0}" = 1 ] && { echo "KEEP=1: instance left running (T=$T DISPLAY=$DPY GUI=$GUI_UNIT daemon-scope=hp-e2e-mw-daemon-$$ xvfb=$XVFB_PID)"; return; }
  systemctl --user stop "hp-e2e-mw-daemon-$$.scope" 2>/dev/null
  [ -n "${GUI_UNIT:-}" ] && systemctl --user stop "$GUI_UNIT.scope" 2>/dev/null
  [ -n "${XVFB_PID:-}" ] && kill "$XVFB_PID" 2>/dev/null
}
trap cleanup EXIT

LIVE_CF=$HOME/.local/state/hyperpanes/control.json
LIVE_SUM=$(sha256sum "$LIVE_CF" 2>/dev/null | cut -d' ' -f1)
rec INFO "binary" "$BIN"
rec INFO "disk" "$(df -BG / | tail -1)"
setup
rec INFO "scratch/display" "T=$T DISPLAY=$DPY"
check "daemon pre-started" alive "$(alive "$(daemon_pid)")"

# ---------------- boot 1: build the scenario ----------------
boot; sleep 2
sleep 1
rec INFO "backend" "$(grep -E 'session-backend' "$T/tmp/hyperpanes-debug.log" | head -1)"
check "no in-process fallback" 0 "$(grep -c 'falling back to in-process' "$T/tmp/hyperpanes-debug.log")"
sleep 2; new_window_via_palette "$(xwa)"
wait_windows 2; check "second window opened via palette" 2 "$(nwin)"
cmd '{"type":"newPane","windowId":1,"pane":{"label":"fake-claude","command":"sh","args":["-c","exec -a fake-claude sleep 100000"],"meta":{"role":"lead","parent":"p-x","team":"t1","someState":"busy"}}}' >/dev/null
sleep 2
FPID=$(fake_pid); rec INFO "fake-claude pid (before)" "$FPID"
check "fake-claude in window 2 (windowId 1)" 1 "$(fake_claude_pane | jq .windowId)"
rec INFO "keep-alive pref" "$(grep -rhs -i 'keepalive\|keep_alive\|keep-alive' "$T/config/hyperpanes" "$T/data" "$T/state" 2>/dev/null | head -3) (absent = default ON; debug log below)"
sleep 7; shot v0-two-windows
check "autosave has 2 windows" 2 "$(jq '.windows|length' "$T/state/hyperpanes/last-workspace.json")"

# ---------------- V1: real close button on window 2 ----------------
echo "=== V1"
W0=$(xwa); W1=$(xwb)
body "$W1"; click_x "$W1"; shot v1-overlay
rec INFO "V1 overlay evidence" "screenshot shots/v1-overlay.png (expect 'This window has 1 running process.' with Cancel / Close window / Quit Hyperpanes)"
key "$W1" Escape; sleep 0.5; shot v1-after-esc
check "V1 Esc cancels: windows still 2" 2 "$(nwin)"
check "V1 Esc: GUI alive" active "$(systemctl --user is-active "$GUI_UNIT.scope")"
click_x "$W1"; shot v1-overlay-2
key "$W1" Return
quit_gui_state V1
boot; sleep 3; nudge; assert_restored V1 2 "$FPID"

[ "${STOP_AFTER:-}" = V1 ] && exit 0
# ---------------- V2: ctrl+shift+q ----------------
echo "=== V2"
W0=$(xwa); rec INFO "V2 windows" "$(for w in $(xwins); do echo "$w@$(wpos $w)"; done | tr "\n" " ")"; body "$W0"; key "$W0" ctrl+shift+q
quit_gui_state V2
boot; sleep 3; nudge; assert_restored V2 2 "$FPID"

# ---------------- V3: close a non-last window ends its panes ----------------
echo "=== V3"
W0=$(xwa); new_window_via_palette "$W0"
wait_windows 3; check "V3 third window opened" 3 "$(nwin)"
cmd '{"type":"newPane","windowId":2,"pane":{"label":"prog3","command":"sleep","args":["100001"]}}' >/dev/null; sleep 2
SPID=$(pgrep -f '^sleep 100001' | head -1); rec INFO "sleep 100001 pid" "$SPID"
check "V3 sleep alive before" alive "$(alive "$SPID")"
W2=$(xwins | tail -1); body "$W2"; click_x "$W2"; shot v3-overlay
click "$W2" 673 439; sleep 2; shot v3-after-close
check "V3 windows after Close window" 2 "$(nwin)"
check "V3 closed window's process ended" dead "$(alive "$SPID")"
check "V3 fake-claude still alive" alive "$(alive "$(fake_pid)")"
check "V3 GUI still running" active "$(systemctl --user is-active "$GUI_UNIT.scope")"
rec INFO "V3 layout" "$(snap_panes)"

# ---------------- V4: last window via X ----------------
echo "=== V4"
# reduce to one window: close window 1 (idle shell only) first
W0=$(xwa); body "$W0"; click_x "$W0"; sleep 1; shot v4-close-other
[ "$(nwin)" = 2 ] && { rec INFO "V4 other window needed confirm" "yes -> Close window"; click "$W0" 673 439; sleep 2; }
check "V4 down to one window" 1 "$(nwin)"
rec INFO "V4 remaining" "$(snap_panes)"
check "V4 claude pane is in the remaining window" fake-claude "$(fake_claude_pane | jq -r '"fake-claude"')"
sleep 7
WL=$(xwins | head -1); body "$WL"; click_x "$WL"; sleep 0.4; shot v4-after-click
rec INFO "V4 overlay" "none expected for the last window (see shots/v4-after-click.png)"
G=$(gui_gone)
if [ "$G" != gone ]; then rec FAIL "V4 app exited on last-window X" "GUI still running (overlay shown?) -> pressing Enter to continue"; key "$WL" Return; fi
quit_gui_state V4
boot; sleep 3; nudge; assert_restored V4 1 "$FPID"

# ---------------- teardown ----------------
LIVE_SUM2=$(sha256sum "$LIVE_CF" 2>/dev/null | cut -d' ' -f1)
check "live control.json untouched" "$LIVE_SUM" "$LIVE_SUM2"
{ echo "# Multi-window restore E2E result"; echo; echo "Generated by e2e.sh; $(date -u +%FT%TZ). Binary: $BIN"; echo
  echo "| status | check | evidence |"; echo "|---|---|---|"
  sed 's/^\([A-Z]*\) | \(.*\) | \(.*\)$/| \1 | \2 | `\3` |/' "$RES"; echo
  echo "FAIL count: $(grep -c '^FAIL' "$RES")"; } >"$EVD/RESULT.md"
cp "$T/tmp/hyperpanes-debug.log" "$EVD/gui-debug.log" 2>/dev/null
grep -q '^FAIL' "$RES" && exit 1; exit 0
