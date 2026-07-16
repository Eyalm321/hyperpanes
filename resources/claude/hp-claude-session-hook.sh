#!/bin/sh
# Claude Code SessionStart / SessionEnd hook -> hyperpanes pane->conversation map.
#
# Register in ~/.claude/settings.json under BOTH events:
#   "hooks": {
#     "SessionStart": [ { "hooks": [ { "type": "command", "command": "<path to this script>" } ] } ],
#     "SessionEnd":   [ { "hooks": [ { "type": "command", "command": "<path to this script>" } ] } ]
#   }
#
# Claude pipes hook JSON (session_id, cwd, hook_event_name, ...) on stdin. When the
# claude runs inside a hyperpanes pane (HYPERPANES_PANE_ID in the pane env), this writes
#   <state dir>/claude-sessions/<pane-id>.json = { "sessionId":..., "cwd":..., "configDir":... }
# on SessionStart and removes it on SessionEnd — so a marker exists exactly while a
# conversation is live in that pane. The GUI's relaunch snapshot embeds the id, letting a
# restored pane `claude --resume` the same conversation in the same directory.
# (Path must mirror hyperpanes-core persistence::paths::claude_sessions_dir.)
#
# Outside a pane, or on any error, this exits 0 silently — a hook must never break claude.
[ -n "$HYPERPANES_PANE_ID" ] || { cat >/dev/null 2>&1; exit 0; }

case "$(uname 2>/dev/null)" in
  Darwin) base="$HOME/Library/Application Support/hyperpanes" ;;
  *)      base="${XDG_STATE_HOME:-$HOME/.local/state}/hyperpanes" ;;
esac

HP_SESS_DIR="$base/claude-sessions" HP_PANE="$HYPERPANES_PANE_ID" python3 -c '
import json, os, sys

try:
    data = json.load(sys.stdin)
except Exception:
    sys.exit(0)
d = os.environ["HP_SESS_DIR"]
path = os.path.join(d, os.environ["HP_PANE"] + ".json")
try:
    if data.get("hook_event_name") == "SessionEnd":
        try:
            os.remove(path)
        except FileNotFoundError:
            pass
    else:
        os.makedirs(d, exist_ok=True)
        tmp = path + ".tmp"
        # configDir records the account this conversation was saved under: `claude` stores
        # transcripts in $CLAUDE_CONFIG_DIR/projects, so a relaunch must set the SAME
        # CLAUDE_CONFIG_DIR for `claude --resume <id>` to find the session (multi-account).
        with open(tmp, "w") as f:
            json.dump({
                "sessionId": data.get("session_id", ""),
                "cwd": data.get("cwd", ""),
                "configDir": os.environ.get("CLAUDE_CONFIG_DIR", ""),
            }, f)
        os.replace(tmp, path)
except OSError:
    pass
' 2>/dev/null
exit 0
