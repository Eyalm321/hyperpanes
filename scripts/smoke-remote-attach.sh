#!/usr/bin/env bash
# Remote-attach smoke: a fully isolated HOST and CLIENT hyperpanes under one Xvfb.
# The client attaches to the host via connectRemote, we check host output reaches the client,
# then type through the real GUI (xdotool) and check it reaches the host pty.
# Usage: scripts/smoke-remote-attach.sh <path-to-hyperpanes-binary> [evidence-dir]
# Docs: docs/remote-attach.md. Isolation rules: docs/dev-instance-isolation.md.
# Env: SMOKE_ALLOW_NO_XDOTOOL=1 lets step 6 SKIP (exit 0) when xdotool is missing.
set -euo pipefail

BIN=${1:?usage: $0 <path-to-hyperpanes-binary> [evidence-dir]}
[ -x "$BIN" ] || { echo "not executable: $BIN" >&2; exit 2; }
BIN=$(readlink -f "$BIN")

# ---- 1. scratch root with two isolated instances (never /tmp: tmpfs quota) ----------------
CACHE=${XDG_CACHE_HOME:-$HOME/.cache}
mkdir -p "$CACHE"
ROOT=$(mktemp -d -p "$CACHE" hp-remote-smoke.XXXXXX)
EVID=${2:-$ROOT/evidence}
mkdir -p "$EVID"
EVID=$(readlink -f "$EVID")

LIVE_DIRS=("$HOME/.local/state/hyperpanes" "$HOME/.local/share/hyperpanes" "$HOME/.config/hyperpanes")
for inst in host client; do
  d=$ROOT/$inst
  mkdir -p "$d/state/hyperpanes" "$d/data" "$d/config/hyperpanes" "$d/run"
  chmod 0700 "$d/run"
  for sub in state data config run; do
    case "$(readlink -f "$d/$sub")/" in
      "$ROOT"/*) ;;
      *) echo "refusing: $d/$sub escapes scratch root" >&2; exit 2 ;;
    esac
    for live in "${LIVE_DIRS[@]}"; do
      case "$(readlink -f "$d/$sub")/" in
        "$live"/*) echo "refusing: $d/$sub is inside live dir $live" >&2; exit 2 ;;
      esac
    done
  done
done

PIDS=()
XPID=""
SUMMARY=$EVID/summary.txt
: > "$SUMMARY"
HOST_TOKEN="" CLIENT_TOKEN="" DEV_TOKEN=""

# Redact known tokens and any token=... before anything reaches the evidence dir.
redact() {
  local s; s=$(cat)
  [ -n "$HOST_TOKEN" ] && s=${s//"$HOST_TOKEN"/REDACTED}
  [ -n "$CLIENT_TOKEN" ] && s=${s//"$CLIENT_TOKEN"/REDACTED}
  [ -n "$DEV_TOKEN" ] && s=${s//"$DEV_TOKEN"/REDACTED}
  printf '%s\n' "$s" | sed -E 's/token=[^&"[:space:]]+/token=REDACTED/g'
}
step() { echo "$1: $2${3:+ ($3)}" | tee -a "$SUMMARY"; }
FAILED=0
fail() { step "$1" FAIL "${2:-}"; FAILED=1; finish; }

cleanup() {
  local rc=$?
  for inst in host client; do
    ( env XDG_STATE_HOME="$ROOT/$inst/state" XDG_DATA_HOME="$ROOT/$inst/data" \
        XDG_CONFIG_HOME="$ROOT/$inst/config" XDG_RUNTIME_DIR="$ROOT/$inst/run" \
        HYPERPANES_CONTROL_FILE="$ROOT/$inst/state/hyperpanes/control.json" \
        "$BIN" --kill-daemon >/dev/null 2>&1 || true )
  done
  for p in "${PIDS[@]:-}"; do [ -n "$p" ] && kill "$p" 2>/dev/null || true; done
  [ -n "$XPID" ] && kill "$XPID" 2>/dev/null || true
  # Keep only redacted copies of logs in the evidence dir; drop raw ones.
  echo "evidence: $EVID"
  exit "$rc"
}
trap cleanup EXIT

finish() {
  for l in host client xvfb; do
    [ -f "$ROOT/$l.log" ] && redact < "$ROOT/$l.log" > "$EVID/$l.log" || true
  done
  if [ "$FAILED" = 0 ]; then echo "OVERALL: PASS" | tee -a "$SUMMARY"; exit 0; fi
  echo "OVERALL: FAIL" | tee -a "$SUMMARY"; exit 1
}

for tool in Xvfb curl jq python3; do
  command -v "$tool" >/dev/null || { echo "missing tool: $tool" >&2; exit 2; }
done

# ---- 2. host config ------------------------------------------------------------------------
free_port() { python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])'; }
HOST_PORT=$(free_port)
cat > "$ROOT/host/config/hyperpanes/control-settings.json" <<JSON
{ "enabled": true, "allowInput": true, "bindAddress": "127.0.0.1", "port": $HOST_PORT }
JSON
cat > "$ROOT/client/config/hyperpanes/control-settings.json" <<JSON
{ "enabled": true, "allowInput": true }
JSON

# ---- 3. Xvfb + both apps -------------------------------------------------------------------
N=90
while [ -e "/tmp/.X11-unix/X$N" ] || [ -e "/tmp/.X$N-lock" ]; do N=$((N + 1)); done
Xvfb ":$N" -screen 0 1600x1000x24 >"$ROOT/xvfb.log" 2>&1 &
XPID=$!
sleep 1
kill -0 "$XPID" 2>/dev/null || { echo "Xvfb failed to start" >&2; cat "$ROOT/xvfb.log" >&2; exit 2; }

launch() { # <instance>
  local inst=$1
  env -u WAYLAND_DISPLAY -u HYPERPANES_PANE_ID DISPLAY=":$N" \
    XDG_STATE_HOME="$ROOT/$inst/state" XDG_DATA_HOME="$ROOT/$inst/data" \
    XDG_CONFIG_HOME="$ROOT/$inst/config" XDG_RUNTIME_DIR="$ROOT/$inst/run" \
    HYPERPANES_CONTROL_FILE="$ROOT/$inst/state/hyperpanes/control.json" \
    "$BIN" >"$ROOT/$inst.log" 2>&1 &
  PIDS+=($!)
}
launch host
launch client

wait_control() { # <instance>
  local cf=$ROOT/$1/state/hyperpanes/control.json
  for _ in $(seq 1 300); do
    [ -s "$cf" ] && jq -e '.port and .token' "$cf" >/dev/null 2>&1 && return 0
    sleep 0.2
  done
  return 1
}
wait_control host || fail "step3-launch-host" "control.json never appeared"
wait_control client || fail "step3-launch-client" "control.json never appeared"
HOST_TOKEN=$(jq -r .token "$ROOT/host/state/hyperpanes/control.json")
HOST_API=$(jq -r .port "$ROOT/host/state/hyperpanes/control.json")
CLIENT_TOKEN=$(jq -r .token "$ROOT/client/state/hyperpanes/control.json")
CLIENT_API=$(jq -r .port "$ROOT/client/state/hyperpanes/control.json")
step "step3-launch" PASS "host:$HOST_API client:$CLIENT_API display :$N"
if find "$ROOT/host/run" "$ROOT/client/run" -mindepth 1 2>/dev/null | grep -q .; then
  step "info-pty-mode" PASS "session daemon runtime sockets present (daemon mode)"
else
  step "info-pty-mode" PASS "no daemon sockets under isolated XDG_RUNTIME_DIR: PTYs run in-process"
fi

# Auth header is passed from a variable via a curl config on stdin, not argv.
api() { # <host|client> <method> <path> [json-body]
  local who=$1 m=$2 p=$3 b=${4:-} tok port
  if [ "$who" = host ]; then tok=$HOST_TOKEN; port=$HOST_API; else tok=$CLIENT_TOKEN; port=$CLIENT_API; fi
  if [ -n "$b" ]; then
    printf 'header = "Authorization: Bearer %s"\n' "$tok" |
      curl -sS -m 15 -K - -X "$m" -H 'content-type: application/json' -d "$b" "http://127.0.0.1:$port$p"
  else
    printf 'header = "Authorization: Bearer %s"\n' "$tok" |
      curl -sS -m 15 -K - -X "$m" "http://127.0.0.1:$port$p"
  fi
}

# ---- 4. host pane, device token, pairing URL, host marker ----------------------------------
host_pane() { api host GET /state | jq -r '[.. | objects | select(has("sessionUid")) | .id] | first // empty'; }
PANE=""
for _ in $(seq 1 50); do PANE=$(host_pane); [ -n "$PANE" ] && break; sleep 0.4; done
if [ -z "$PANE" ]; then
  WID=$(api host GET /state | jq -r '.windows[0].windowId // empty')
  [ -n "$WID" ] && api host POST /command "{\"type\":\"newPane\",\"windowId\":$WID,\"pane\":{\"label\":\"smoke\"}}" >/dev/null || true
  for _ in $(seq 1 25); do PANE=$(host_pane); [ -n "$PANE" ] && break; sleep 0.4; done
fi
[ -n "$PANE" ] || fail "step4-host-pane" "no host pane"

# Same request `hyperpanes pair` sends: POST /devices {label, ttlMs?} with the master token.
DEV_TOKEN=$(api host POST /devices '{"label":"smoke-client","ttlMs":3600000}' | jq -r .token)
[ -n "$DEV_TOKEN" ] && [ "$DEV_TOKEN" != null ] || fail "step4-mint" "no device token"
PAIR_URL="hp://127.0.0.1:$HOST_PORT/?token=$DEV_TOKEN&v=1"

HOST_MARKER="HOST_MARKER_$RANDOM"
api host POST "/panes/$PANE/input" "{\"data\":\"echo $HOST_MARKER\\r\"}" >/dev/null
step "step4-host-setup" PASS "pane $PANE, device token minted"

# ---- 5. client connects; host marker must show in remoteScreen ------------------------------
RESP=$(api client POST /command "$(jq -nc --arg u "$PAIR_URL" '{type:"connectRemote",url:$u}')") || true
echo "$RESP" | jq -e '.ok == true' >/dev/null 2>&1 || fail "step5-connect" "connectRemote failed: $(echo "$RESP" | redact | head -c 300)"
OK5=0
for _ in $(seq 1 60); do
  SCR=$(api client POST /command '{"type":"remoteScreen"}' || true)
  if echo "$SCR" | jq -e --arg m "$HOST_MARKER" '[.. | .screenText? // empty | select(contains($m))] | length > 0' >/dev/null 2>&1; then OK5=1; break; fi
  sleep 0.5
done
[ "$OK5" = 1 ] || { echo "$SCR" | redact > "$EVID/client-remoteScreen.json"; fail "step5-host-to-client" "marker not seen on client within 30s"; }
step "step5-host-to-client" PASS

# ---- 6. typed input through the real GUI path ------------------------------------------------
if ! command -v xdotool >/dev/null; then
  step "step6-client-to-host" SKIP "xdotool missing"
  if [ "${SMOKE_ALLOW_NO_XDOTOOL:-0}" != 1 ]; then FAILED=1; fi
else
  CLIENT_MARKER="CLIENT_MARKER_$RANDOM"
  export DISPLAY=":$N"
  # Both apps share one Xvfb with no window manager, so stacking is map order. Make the click
  # unambiguous: shrink the HOST window into a corner and put the CLIENT window top-left, raised.
  # (A marker typed into the host GUI by mistake would otherwise be a false PASS.)
  win_of() { # <pid>: first visible window of that process (retry while it maps)
    local w=""
    for _ in $(seq 1 50); do
      w=$(xdotool search --onlyvisible --pid "$1" 2>/dev/null | head -1 || true)
      [ -n "$w" ] && break
      sleep 0.2
    done
    echo "$w"
  }
  HOST_WIN=$(win_of "${PIDS[0]}"); CLIENT_WIN=$(win_of "${PIDS[1]}")
  [ -n "$HOST_WIN" ] && [ -n "$CLIENT_WIN" ] || fail "step6-windows" "could not find GUI windows (host=$HOST_WIN client=$CLIENT_WIN)"
  xdotool windowsize "$HOST_WIN" 240 160 windowmove "$HOST_WIN" 1350 820 || true
  xdotool windowmove "$CLIENT_WIN" 0 0 windowsize "$CLIENT_WIN" 1200 800 windowraise "$CLIENT_WIN" || true
  # No window manager → no X input focus management: focus the client window explicitly.
  xdotool windowfocus "$CLIENT_WIN" || true
  sleep 1
  # winit drops XTEST keys until a real click lands in the window: click inside the client's
  # pane area (the remote tab is activated by connectRemote), twice for good measure.
  xdotool mousemove 500 450 click 1; sleep 0.5
  xdotool mousemove 520 470 click 1; sleep 0.5
  xdotool type --delay 40 "echo $CLIENT_MARKER"; xdotool key Return
  OK6=0
  for _ in $(seq 1 60); do
    if api host GET "/panes/$PANE/output?strip=1" | jq -e --arg m "$CLIENT_MARKER" '.output | contains($m)' >/dev/null 2>&1; then OK6=1; break; fi
    sleep 0.5
  done
  if [ "$OK6" = 1 ]; then step "step6-client-to-host" PASS; else step "step6-client-to-host" FAIL "marker not on host within 30s"; FAILED=1; fi
fi

# ---- 7. evidence (redacted) ------------------------------------------------------------------
DISPLAY=":$N" import -window root "$EVID/screenshot.png" 2>/dev/null ||
  { command -v xwd >/dev/null && DISPLAY=":$N" xwd -root | convert xwd:- "$EVID/screenshot.png" 2>/dev/null; } ||
  echo "no screenshot tool" >> "$SUMMARY"
api host GET /state | redact > "$EVID/host-state.json" || true
api client POST /command '{"type":"remoteScreen"}' | redact > "$EVID/client-remoteScreen.json" || true
finish
