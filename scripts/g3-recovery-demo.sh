#!/usr/bin/env bash
# End-to-end proof of the agent-pane API-error recovery contract
# (docs/agent-recovery.md): detect -> classify -> repair -> resume, against a REAL
# poisoned Claude Code transcript.
#
# Boots an ISOLATED headless hyperpanes-core instance (its own XDG_STATE_HOME /
# XDG_DATA_HOME / XDG_CONFIG_HOME, its own ephemeral port+token) — it never touches the
# user's running GUI app or its control.json. The one thing it does NOT isolate is
# Claude Code's own `~/.claude` store: a poisoned session has to be resumable by the
# REAL `claude` CLI, so it is installed there, under a throwaway project path that is
# unique per run (mktemp + uuidgen) and removed in cleanup — never anything a real
# session could collide with.
#
# Usage: scripts/g3-recovery-demo.sh
# Prints PASS/FAIL per step; exits 0 only if every step passed.
set -u

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CORE_DIR="$REPO_ROOT/rs"
FIXTURE="$CORE_DIR/crates/core/tests/fixtures/g2-poisoned-transcript.jsonl"

PASS_COUNT=0
FAIL_COUNT=0
ok() {
    echo "PASS: $1"
    PASS_COUNT=$((PASS_COUNT + 1))
}
fail() {
    echo "FAIL: $1"
    FAIL_COUNT=$((FAIL_COUNT + 1))
}

# Keep [A-Za-z0-9], map every other char to '-' — Claude Code's own project-dir encoding
# (mirrors claude_history::encode_path_str).
encode() {
    printf '%s' "$1" | sed -E 's/[^A-Za-z0-9]/-/g'
}

if [ ! -f "$FIXTURE" ]; then
    echo "FAIL: fixture not found: $FIXTURE"
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "FAIL: jq is required"
    exit 1
fi
if ! command -v uuidgen >/dev/null 2>&1; then
    echo "FAIL: uuidgen is required"
    exit 1
fi
if ! command -v claude >/dev/null 2>&1; then
    echo "FAIL: claude CLI is required and must be authenticated"
    exit 1
fi

TMP="$(mktemp -d)"
SID="$(uuidgen)"
PROJ="$TMP/proj"
mkdir -p "$PROJ"

XDG_STATE_HOME="$TMP/xstate"
XDG_DATA_HOME="$TMP/xdata"
XDG_CONFIG_HOME="$TMP/xconfig"
mkdir -p "$XDG_STATE_HOME" "$XDG_DATA_HOME" "$XDG_CONFIG_HOME"
CONTROL_JSON="$XDG_STATE_HOME/hyperpanes/control.json"

# ---- production-state guard (hard requirement, see docs/agent-recovery.md) ----------------
# The headless boot MUST be pointed away from the live org's control file EXPLICITLY:
# HYPERPANES_CONTROL_FILE takes precedence over the XDG-derived path (app.rs:44-46), and this
# very script inherited it pointing at production three times before this guard existed. A test
# that CAN reach production state eventually WILL.
LIVE_CONTROL_JSON="$HOME/.local/state/hyperpanes/control.json"
if [ "$CONTROL_JSON" = "$LIVE_CONTROL_JSON" ]; then
    echo "FAIL: refusing to run — isolated control path equals the live control file ($LIVE_CONTROL_JSON)"
    exit 1
fi
# Snapshot the live control file (if present) so the end of the run can PROVE it was untouched.
LIVE_SNAPSHOT=""
if [ -f "$LIVE_CONTROL_JSON" ]; then
    LIVE_SNAPSHOT="$TMP/live-control-before.json"
    cp "$LIVE_CONTROL_JSON" "$LIVE_SNAPSHOT"
fi

ENCODED_PROJ="$(encode "$PROJ")"
# The account the poisoned session lives under. It must be an account whose MCP tool set
# matches what the fixture conversation actually loaded (tokensave/serena/headroom/hyperpanes)
# — the g2 incident session ran under the goals-rotation account `.claude-sunsations`, and a
# resume in a store without those servers fails with a NEW 400 ("Tool reference ... not found
# in available tools") even after the poison is excised. The headless server's markerless scan
# finds this store via claude_accounts discovery (a `.claude*` home dir with .credentials.json).
ACCOUNT_DIR="${HP_DEMO_CLAUDE_CONFIG_DIR:-$HOME/.claude-sunsations}"
if [ ! -d "$ACCOUNT_DIR" ]; then
    echo "FAIL: account dir $ACCOUNT_DIR does not exist (set HP_DEMO_CLAUDE_CONFIG_DIR)"
    exit 1
fi
DEFAULT_STORE_DIR="$ACCOUNT_DIR/projects/$ENCODED_PROJ"
DEST="$DEFAULT_STORE_DIR/$SID.jsonl"

HEADLESS_PID=""
cleanup() {
    if [ -n "$HEADLESS_PID" ] && kill -0 "$HEADLESS_PID" 2>/dev/null; then
        kill "$HEADLESS_PID" 2>/dev/null
        for _ in $(seq 1 20); do
            kill -0 "$HEADLESS_PID" 2>/dev/null || break
            sleep 0.2
        done
        kill -9 "$HEADLESS_PID" 2>/dev/null
    fi
    rm -rf "$TMP"
    rm -rf "$DEFAULT_STORE_DIR"
}
trap cleanup EXIT

echo "== step 1: install the poisoned session into the account transcript store =="
mkdir -p "$DEFAULT_STORE_DIR"
FIXTURE_SID="$(grep -o '"sessionId":"[^"]*"' "$FIXTURE" | head -1 | sed -E 's/.*:"([^"]*)"/\1/')"
if [ -z "$FIXTURE_SID" ]; then
    fail "step 1: could not find a sessionId in the fixture"
    exit 1
fi
sed "s/$FIXTURE_SID/$SID/g" "$FIXTURE" >"$DEST"
if [ -f "$DEST" ]; then
    ok "step 1: installed $DEST (fixture sessionId $FIXTURE_SID -> $SID)"
else
    fail "step 1: failed to write $DEST"
    exit 1
fi

echo "== step 2: prove the poison is live =="
STEP2_OUT="$(cd "$PROJ" && env CLAUDE_CONFIG_DIR="$ACCOUNT_DIR" claude --resume "$SID" -p "reply with exactly OK" --model claude-haiku-4-5-20251001 2>&1)"
STEP2_STATUS=$?
printf '%s\n' "$STEP2_OUT" >"$TMP/err.txt"
if [ $STEP2_STATUS -ne 0 ] && printf '%s' "$STEP2_OUT" | grep -q "API Error" && printf '%s' "$STEP2_OUT" | grep -q "tool_use_id"; then
    ok "step 2: claude --resume failed with an API Error mentioning tool_use_id, as expected"
else
    fail "step 2: expected a failing API Error mentioning tool_use_id (exit=$STEP2_STATUS)"
    echo "--- claude --resume output ---"
    printf '%s\n' "$STEP2_OUT"
    echo "-------------------------------"
fi

echo "== step 3: boot an isolated headless hyperpanes-core instance =="
# Build outside /tmp: worktrees live on a tmpfs here, and a fresh target/ inside one costs RAM.
CARGO_TARGET_DIR="${HP_DEMO_TARGET_DIR:-$HOME/.cache/hyperpanes-g3-demo-target}"
export CARGO_TARGET_DIR
(cd "$CORE_DIR" && cargo build -p hyperpanes-core --bin headless) >"$TMP/build.log" 2>&1
if [ $? -ne 0 ]; then
    fail "step 3: cargo build failed — see $TMP/build.log"
    cat "$TMP/build.log"
    exit 1
fi
HEADLESS_BIN="$CARGO_TARGET_DIR/debug/headless"
# HYPERPANES_CONTROL_FILE must be overridden HERE: it wins over XDG_STATE_HOME (app.rs:44-46)
# and the surrounding environment carries it pointing at the live org's control file.
XDG_STATE_HOME="$XDG_STATE_HOME" XDG_DATA_HOME="$XDG_DATA_HOME" XDG_CONFIG_HOME="$XDG_CONFIG_HOME" \
    HYPERPANES_CONTROL_FILE="$CONTROL_JSON" \
    HYPERPANES_ALLOW_INPUT=1 "$HEADLESS_BIN" >"$TMP/headless.log" 2>&1 &
HEADLESS_PID=$!

for _ in $(seq 1 60); do
    [ -f "$CONTROL_JSON" ] && break
    sleep 0.5
done
if [ ! -f "$CONTROL_JSON" ]; then
    fail "step 3: isolated headless instance never wrote $CONTROL_JSON (possible single-instance collision with another running headless — see $TMP/headless.log)"
    cat "$TMP/headless.log" 2>/dev/null
    exit 1
fi

PORT="$(jq -r .port "$CONTROL_JSON")"
TOKEN="$(jq -r .token "$CONTROL_JSON")"
BASE="http://127.0.0.1:$PORT"

api() {
    curl -sS -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" "$@"
}

HEALTH="$(api "$BASE/health")"
if printf '%s' "$HEALTH" | jq -e '.ok == true' >/dev/null 2>&1; then
    ok "step 3: isolated control server up at $BASE (pid $HEADLESS_PID)"
else
    fail "step 3: isolated control server did not respond healthy: $HEALTH"
    exit 1
fi

echo "== step 4: open a pane holding the captured API-Error text, then inspect =="
STATE="$(api "$BASE/state")"
WINDOW_ID="$(printf '%s' "$STATE" | jq -r '.windows[0].windowId')"
if [ -z "$WINDOW_ID" ] || [ "$WINDOW_ID" = "null" ]; then
    fail "step 4: no window to spawn the demo pane into: $STATE"
    exit 1
fi

NEWPANE_BODY="$(jq -n --arg cwd "$PROJ" --arg cmd "cat $TMP/err.txt; exec sh" --argjson windowId "$WINDOW_ID" \
    '{type: "newPane", windowId: $windowId, pane: {command: $cmd, cwd: $cwd}}')"
NEWPANE_RESP="$(api -X POST "$BASE/command" -d "$NEWPANE_BODY")"
PANE_ID="$(printf '%s' "$NEWPANE_RESP" | jq -r '.result // empty')"
if [ -z "$PANE_ID" ]; then
    fail "step 4: newPane failed: $NEWPANE_RESP"
    exit 1
fi
ok "step 4: opened demo pane $PANE_ID (cwd=$PROJ)"

# Let the shell settle (cat the error text, drop to a prompt) before inspecting.
sleep 1.5

INSPECT_BODY="$(jq -n --arg paneId "$PANE_ID" '{type: "recoverPane", paneId: $paneId, action: "inspect"}')"
INSPECT_RESP="$(api -X POST "$BASE/command" -d "$INSPECT_BODY")"
CLASS="$(printf '%s' "$INSPECT_RESP" | jq -r '.result.class // empty')"
CANDIDATE_HIT="$(printf '%s' "$INSPECT_RESP" | jq --arg sid "$SID" '.result.session.candidates // [] | map(select(.sessionId == $sid)) | .[0] // empty')"
CANDIDATE_CONFIG_DIR="$(printf '%s' "$CANDIDATE_HIT" | jq -r '.configDir // "null"' 2>/dev/null)"

if [ "$CLASS" = "poisoned" ]; then
    ok "step 4: recoverPane inspect classified the tail as poisoned"
else
    fail "step 4: expected class=poisoned, got: $INSPECT_RESP"
fi
# The session was installed under an ACCOUNT store (not ~/.claude), so the candidate must
# carry a non-null configDir — that account linkage is what lets resume run under the right
# CLAUDE_CONFIG_DIR. (~/.claude-sunsations/projects symlinks to the shared store, so the
# reported dir may be either account name; both resolve to the store the file lives in.)
if [ -n "$CANDIDATE_HIT" ] && [ "$CANDIDATE_HIT" != "null" ] && [ "$CANDIDATE_CONFIG_DIR" != "null" ] && [ -n "$CANDIDATE_CONFIG_DIR" ]; then
    ok "step 4: scan candidates include $SID with configDir=$CANDIDATE_CONFIG_DIR (markerless, account-carrying resolution proved)"
else
    fail "step 4: expected a markerless scan candidate for $SID with a non-null configDir, got: $INSPECT_RESP"
fi

echo "== step 5: repair the poisoned transcript =="
REPAIR_BODY="$(jq -n --arg paneId "$PANE_ID" '{type: "recoverPane", paneId: $paneId, action: "repair"}')"
REPAIR_RESP="$(api -X POST "$BASE/command" -d "$REPAIR_BODY")"
DROPPED="$(printf '%s' "$REPAIR_RESP" | jq -c '.result.dropped // empty')"
BACKUP="$(printf '%s' "$REPAIR_RESP" | jq -r '.result.backup // empty')"
if [ "$DROPPED" = "[9]" ] && [ -n "$BACKUP" ] && [ -f "$BACKUP" ]; then
    ok "step 5: repair dropped line 9 and wrote a backup at $BACKUP"
else
    fail "step 5: expected dropped==[9] and an existing backup, got: $REPAIR_RESP"
fi

echo "== step 6: re-run the resume — must now succeed =="
STEP6_OUT="$(cd "$PROJ" && env CLAUDE_CONFIG_DIR="$ACCOUNT_DIR" claude --resume "$SID" -p "reply with exactly OK" --model claude-haiku-4-5-20251001 2>&1)"
STEP6_STATUS=$?
if [ $STEP6_STATUS -eq 0 ] && printf '%s' "$STEP6_OUT" | grep -q "OK"; then
    ok "step 6: claude --resume succeeded and replied OK — the repaired session is usable"
else
    fail "step 6: expected a successful --resume containing OK (exit=$STEP6_STATUS)"
    echo "--- claude --resume output ---"
    printf '%s\n' "$STEP6_OUT"
    echo "-------------------------------"
fi

echo "== (optional) exercise recoverPane action:resume on the demo pane =="
RESUME_BODY="$(jq -n --arg paneId "$PANE_ID" '{type: "recoverPane", paneId: $paneId, action: "resume"}')"
RESUME_RESP="$(api -X POST "$BASE/command" -d "$RESUME_BODY")"
echo "recoverPane resume result (informational only — step 6 above is the acceptance-level proof): $RESUME_RESP"

echo "== cleanup =="
CLOSE_BODY="$(jq -n --arg paneId "$PANE_ID" '{type: "closePane", paneId: $paneId}')"
api -X POST "$BASE/command" -d "$CLOSE_BODY" >/dev/null 2>&1
ok "cleanup: closed demo pane, isolated instance + temp dirs removed on exit"

echo "== step 7: prove the live control plane was untouched =="
if [ -n "$LIVE_SNAPSHOT" ]; then
    if cmp -s "$LIVE_SNAPSHOT" "$LIVE_CONTROL_JSON"; then
        ok "step 7: live $LIVE_CONTROL_JSON is byte-identical to its pre-run snapshot"
    else
        fail "step 7: live control.json CHANGED during the run — this demo hijacked the org's control plane"
        diff "$LIVE_SNAPSHOT" "$LIVE_CONTROL_JSON" || true
    fi
else
    ok "step 7: no live control.json existed before the run (nothing to protect)"
fi

echo
echo "==================== SUMMARY ===================="
echo "PASS: $PASS_COUNT   FAIL: $FAIL_COUNT"
if [ "$FAIL_COUNT" -eq 0 ]; then
    echo "RESULT: PASS"
    exit 0
else
    echo "RESULT: FAIL"
    exit 1
fi
