# shared helpers for e2e.sh (sourced)
BIN=${BIN:-$CARGO_TARGET_DIR/debug/hyperpanes}
EVD=${EVD:-${XDG_CACHE_HOME:-$HOME/.cache}/hyperpanes/e2e-multiwindow}
T=${T:-$EVD/run/t}
UID_=$(id -u)
setup() {
  rm -rf "$T"; mkdir -p "$T"/{home,state,data,config/hyperpanes,run,tmp}
  chmod 700 "$T/run"; ln -s /run/user/$UID_/systemd "$T/run/systemd"
  echo '{"enabled":true,"allowInput":true}' >"$T/config/hyperpanes/control-settings.json"
  CF=$T/control.json
  for n in $(seq 97 130); do [ -e /tmp/.X11-unix/X$n ] || [ -e /tmp/.X$n-lock ] || { DPY=:$n; break; }; done
  Xvfb $DPY -screen 0 1600x1000x24 -nolisten tcp >"$T/xvfb.log" 2>&1 & XVFB_PID=$!
  for _ in $(seq 50); do [ -e /tmp/.X11-unix/X${DPY#:} ] && break; sleep 0.1; done
  ENVV=(HOME=$T/home HYPERPANES_CONTROL_FILE=$CF XDG_STATE_HOME=$T/state XDG_DATA_HOME=$T/data
    XDG_CONFIG_HOME=$T/config XDG_RUNTIME_DIR=$T/run TMPDIR=$T/tmp DISPLAY=$DPY HYPERPANES_DEBUG=1
    DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$UID_/bus)
  UNSET=(-u WAYLAND_DISPLAY -u HYPERPANES_PANE_ID -u HYPERPANES_CONTROL_TOKEN -u HYPERPANES_CONTROL_PORT)
  XDG_RUNTIME_DIR=/run/user/$UID_ systemd-run --user --scope --quiet --slice=app-graphical.slice --unit=hp-e2e-mw-daemon-$$ \
    env "${UNSET[@]}" "${ENVV[@]}" "$BIN" --session-daemon "$T/config/hyperpanes" >>"$T/daemon.log" 2>&1 &
  sleep 2
}
boot() {
  rm -f "$CF"; GUI_UNIT=hp-e2e-mw-gui-$$-$RANDOM
  XDG_RUNTIME_DIR=/run/user/$UID_ systemd-run --user --scope --quiet --slice=app-graphical.slice --unit=$GUI_UNIT \
    env "${UNSET[@]}" "${ENVV[@]}" "$BIN" >>"$T/gui.log" 2>&1 &
  for _ in $(seq 150); do [ -s "$CF" ] && break; sleep 0.2; done
  PORT=$(jq -r .port "$CF"); TOKEN=$(jq -r .token "$CF")
  for _ in $(seq 100); do n=$(st | jq '[.windows[].tabs[]]|length' 2>/dev/null); [ "${n:-0}" -gt 0 ] && break; sleep 0.2; done
}
st() { curl -s -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:$PORT/state"; }
cmd() { curl -s -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' -X POST "http://127.0.0.1:$PORT/command" -d "$1"; echo; }
shot() { import -display $DPY -window root "$EVD/$1.png" 2>/dev/null; }
