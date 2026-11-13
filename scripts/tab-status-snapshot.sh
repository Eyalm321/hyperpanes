#!/usr/bin/env bash
# Tab-dot consistency snapshot: samples GET /state and checks, for every tab in every sample, that
# the tab's status (what the tab dot and MCP list_panes' tabStatus show) is exactly
# "working iff at least one of the tab's panes reads activity=busy" in the SAME document.
#
# list_panes is a pure projection of one /state document (pane.activity + tab.status), so a
# consistent /state means list_panes is consistent too. The GUI dot runs the same
# control::server::activity_for over the same panes (paneview::tab_status_for).
#
# Usage: scripts/tab-status-snapshot.sh [--json] [samples=30] [interval_s=1]
#   --json  print one JSON document (every sample, every tab, every pane's activity) instead of
#           the table, for attaching as evidence.
# Env:   HYPERPANES_CONTROL_FILE  discovery file (default: $XDG_STATE_HOME/hyperpanes/control.json)
# Read-only: it only issues GET /state. It never prints the control token.
# Exit:  0 = every sample consistent, 1 = at least one mismatch, 2 = cannot reach the API.
set -euo pipefail

JSON=0
ARGS=()
for a in "$@"; do
  case "$a" in
    --json) JSON=1 ;;
    -h|--help) sed -n '2,15p' "$0"; exit 0 ;;
    *) ARGS+=("$a") ;;
  esac
done
SAMPLES=${ARGS[0]:-30}
INTERVAL=${ARGS[1]:-1}
CONTROL=${HYPERPANES_CONTROL_FILE:-${XDG_STATE_HOME:-$HOME/.local/state}/hyperpanes/control.json}
[ -r "$CONTROL" ] || { echo "no control file: $CONTROL" >&2; exit 2; }

exec python3 -I - "$CONTROL" "$SAMPLES" "$INTERVAL" "$JSON" <<'PY'
import json, sys, time, urllib.request

control, samples, interval, as_json = sys.argv[1], int(sys.argv[2]), float(sys.argv[3]), sys.argv[4] == "1"
with open(control) as f:
    disc = json.load(f)
req = urllib.request.Request(
    f"http://127.0.0.1:{disc['port']}/state",
    headers={"Authorization": f"Bearer {disc['token']}"},
)
app_version = disc.get("version")
del disc


def fetch():
    with urllib.request.urlopen(req, timeout=5) as r:
        return json.load(r)


def pane_name(p):
    return f"{p.get('label') or '?'} ({p.get('id', '?')[:8]})"


out = []
mismatches = 0
for n in range(1, samples + 1):
    try:
        state = fetch()
    except Exception as e:  # never echo the request (it carries the token)
        print(f"cannot read /state: {type(e).__name__}", file=sys.stderr)
        sys.exit(2)
    at_ms = int(time.time() * 1000)
    tabs = []
    for w in state.get("windows", []):
        for t in w.get("tabs", []):
            panes = t.get("panes", [])
            busy = [p for p in panes if p.get("activity") == "busy"]
            derived = "working" if busy else "idle"
            ok = t.get("status") == derived
            mismatches += 0 if ok else 1
            tabs.append({
                "windowId": w.get("windowId"),
                "tabId": t.get("id"),
                "title": t.get("title", ""),
                "status": t.get("status"),
                "derived": derived,
                "consistent": ok,
                "panes": [
                    {"paneId": p.get("id"), "label": p.get("label"), "activity": p.get("activity")}
                    for p in panes
                ],
            })
            if not as_json:
                at = time.strftime("%H:%M:%S", time.localtime(at_ms / 1000))
                flag = "ok      " if ok else "MISMATCH"
                names = ", ".join(pane_name(p) for p in busy) or "-"
                print(f"{at} #{n:<3} {flag} tab {t.get('id', '?'):<6} {t.get('status') or '?':<8} "
                      f"{t.get('title', '')[:28]:<28} busy: {names}")
    out.append({"sample": n, "atMs": at_ms, "tabs": tabs})
    if n < samples:
        time.sleep(interval)

if as_json:
    json.dump({"appVersion": app_version, "samples": out, "mismatches": mismatches}, sys.stdout, indent=1)
    print()
else:
    print(f"{samples} sample(s), {mismatches} mismatch(es)")
sys.exit(1 if mismatches else 0)
PY
