#!/usr/bin/env bash
# Scratch debug: spawn a zsh pane via control API on an isolated instance, dump its state + screen.
set -u
ISO=/tmp/hp-dbg
rm -rf "$ISO"
mkdir -p "$ISO/cfg/hyperpanes" "$ISO/state" "$ISO/data"
printf '%s' '{ "enabled": true, "allowInput": true }' > "$ISO/cfg/hyperpanes/control-settings.json"
cd "$(dirname "$0")/.."   # rs/
env -u HYPERPANES_CONTROL_FILE -u HYPERPANES_PANE_ID \
  XDG_CONFIG_HOME=$ISO/cfg XDG_STATE_HOME=$ISO/state XDG_DATA_HOME=$ISO/data \
  nohup crates/app/target/debug/hyperpanes > "$ISO/app.log" 2>&1 &
echo "app pid $!" ; echo $! > "$ISO/pid.txt"
sleep 7
CJ=$ISO/state/hyperpanes/control.json
TOK=$(python3 -c "import json;print(json.load(open('$CJ'))['token'])")
PORT=$(python3 -c "import json;print(json.load(open('$CJ'))['port'])")
WIN=$(curl -s -H "Authorization: Bearer $TOK" "http://127.0.0.1:$PORT/state" \
  | python3 -c "import json,sys;print(json.load(sys.stdin)['windows'][0]['windowId'])")
PANE=$(curl -s -X POST -H "Authorization: Bearer $TOK" -H 'Content-Type: application/json' \
  -d "{\"type\":\"newPane\",\"windowId\":$WIN,\"pane\":{\"shell\":\"zsh\",\"label\":\"dbg\"}}" \
  "http://127.0.0.1:$PORT/command" \
  | python3 -c "import json,sys;print(json.load(sys.stdin).get('result'))")
echo "paneId=$PANE"
sleep 4
echo '--- pane state ---'
curl -s -H "Authorization: Bearer $TOK" "http://127.0.0.1:$PORT/state" \
  | python3 -m json.tool | grep -B2 -A10 '"dbg"'
echo '--- screen ---'
curl -s -H "Authorization: Bearer $TOK" "http://127.0.0.1:$PORT/panes/$PANE/output?mode=screen" | head -c 800
echo
echo "$TOK $PORT $WIN $PANE" > "$ISO/session.txt"
