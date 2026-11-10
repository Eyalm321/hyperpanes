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

SECRET_PREFIXES = ("sk-", "ghp_", "gho_", "ghs_", "github_pat_", "xoxb-", "xoxp-", "AKIA", "AIza")
MAX_ARGV_JSON = 4096


def read_cmdline(pid, proc):
    try:
        with open("%s/%s/cmdline" % (proc, pid), "rb") as f:
            raw = f.read()
    except OSError:
        return None
    parts = raw.split(b"\0")
    if parts and parts[-1] == b"":
        parts.pop()
    return [p.decode("utf-8", "replace") for p in parts] or None


def read_ppid(pid, proc):
    try:
        with open("%s/%s/stat" % (proc, pid)) as f:
            text = f.read()
        return int(text[text.rindex(")") + 1:].split()[1])
    except (OSError, ValueError, IndexError):
        return None


def is_claude(argv):
    if not argv:
        return False
    base = argv[0].replace("\\", "/").rsplit("/", 1)[-1]
    if base in ("claude", "claude.exe") or "/claude/versions/" in argv[0]:
        return True
    return base == "node" and len(argv) > 1 and argv[1].endswith("/claude")


def find_claude(start, proc):
    pid = start
    for _ in range(64):
        if not pid or pid <= 1:
            return None
        argv = read_cmdline(pid, proc)
        if argv and is_claude(argv):
            return argv
        pid = read_ppid(pid, proc)
    return None


def inline_json(v):
    return v.startswith("{") or v.startswith("[")


def has_secret(tokens):
    for i, t in enumerate(tokens):
        if t.startswith(SECRET_PREFIXES) or "-----BEGIN" in t:
            return True
        if "=" in t:
            f, v = t.split("=", 1)
            if f in ("--settings", "--mcp-config", "--agents") and inline_json(v):
                return True
        elif t in ("--settings", "--agents"):
            if i + 1 < len(tokens) and inline_json(tokens[i + 1]):
                return True
        elif t == "--mcp-config":
            j = i + 1
            while j < len(tokens) and not tokens[j].startswith("-"):
                if inline_json(tokens[j]):
                    return True
                j += 1
    return False


def choose(full, existing):
    """What to record for a claude with argv `full`: {"argv": [...]}, {"argvUnavailable": why},
    or (fork workers) whatever the existing marker already holds."""
    # `node /path/to/claude ...` carries the script as its second element.
    skip = 2 if full[0].replace("\\", "/").rsplit("/", 1)[-1] == "node" else 1
    tokens = full[skip:]
    if "--fork-session" in tokens or (len(full) > 1 and full[1] in ("bg-pty-host", "bg-spare")):
        out = {}
        for k in ("argv", "argvUnavailable"):
            if existing and k in existing:
                out[k] = existing[k]
        return out
    if "-p" in tokens or "--print" in tokens:
        return {"argvUnavailable": "print"}
    if has_secret(tokens):
        return {"argvUnavailable": "secret"}
    if len(json.dumps(tokens)) > MAX_ARGV_JSON:
        return {"argvUnavailable": "too-long"}
    return {"argv": tokens}


def capture(proc, start, existing, platform):
    if not platform.startswith("linux"):
        return {"argvUnavailable": "unsupported"}
    full = find_claude(start, proc)
    if full is None:
        return {"argvUnavailable": "unsupported"}
    return choose(full, existing)


def main():
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
            marker = {
                "sessionId": data.get("session_id", ""),
                "cwd": data.get("cwd", ""),
                # configDir records the account this conversation was saved under: `claude`
                # stores transcripts in $CLAUDE_CONFIG_DIR/projects, so a relaunch must set the
                # SAME CLAUDE_CONFIG_DIR for `claude --resume <id>` to find the session.
                "configDir": os.environ.get("CLAUDE_CONFIG_DIR", ""),
            }
            try:
                existing = None
                try:
                    with open(path) as f:
                        existing = json.load(f)
                except (OSError, ValueError):
                    pass
                marker.update(capture("/proc", os.getppid(), existing, sys.platform))
            except Exception:
                pass
            with open(tmp, "w") as f:
                json.dump(marker, f)
            os.replace(tmp, path)
    except OSError:
        pass


if os.environ.get("HP_HOOK_TEST") != "1":
    main()
' 2>/dev/null
exit 0
