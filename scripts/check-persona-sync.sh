#!/usr/bin/env bash
# Fail when the goal-orchestrator personas bundled here have drifted from the skills repo.
#
#   scripts/check-persona-sync.sh [skills-ref]      # default ref: main
#
# The personas exist twice: github.com/Eyalm321/agent-orchestration-skills is the documented
# source of truth (and what ~/.claude/skills/goal-orchestrator symlinks to), while
# resources/claude/goal-orchestrator/ is the snapshot the installed app actually ships and
# spawns goal agents with. Nothing kept the two in step, and twice the bundle pulled ahead by
# 60+ lines of operational fixes that never reached the skills repo (the second time: the
# --strict-mcp-config, --base and watchdog guidance). This turns that drift into a red check.
#
# README.md is deliberately excluded — it describes the bundled snapshot itself.
#
# Exit 0 in sync · 1 drifted · 2 could not fetch (network/ref), so a fetch failure is never
# reported as drift.

set -euo pipefail

REF="${1:-main}"
REPO="Eyalm321/agent-orchestration-skills"
REMOTE_DIR="skills/goal-orchestrator"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LOCAL_DIR="$ROOT/resources/claude/goal-orchestrator"
FILES=(SKILL.md SPEC.md IMPL.md)

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

drifted=()
for f in "${FILES[@]}"; do
  url="https://raw.githubusercontent.com/$REPO/$REF/$REMOTE_DIR/$f"
  if ! curl -fsSL --retry 3 --retry-delay 2 -o "$tmp/$f" "$url"; then
    echo "::error::could not fetch $url" >&2
    exit 2
  fi
  [ -f "$LOCAL_DIR/$f" ] || { echo "::error::bundled persona missing: $LOCAL_DIR/$f" >&2; exit 1; }
  if ! cmp -s "$tmp/$f" "$LOCAL_DIR/$f"; then
    drifted+=("$f")
  fi
done

if [ ${#drifted[@]} -eq 0 ]; then
  echo "personas in sync with $REPO@$REF: ${FILES[*]}"
  exit 0
fi

for f in "${drifted[@]}"; do
  echo "::error file=resources/claude/goal-orchestrator/$f::$f differs from $REPO@$REF/$REMOTE_DIR/$f"
  echo "----- diff: $REPO@$REF ($f)  vs  bundled ($f) -----"
  diff -u --label "skills-repo/$f" --label "bundled/$f" "$tmp/$f" "$LOCAL_DIR/$f" | head -60 || true
done

cat >&2 <<EOF

Bundled goal-orchestrator personas have drifted from $REPO.
Make the same edit in both places, then re-run. To copy the bundle over the skills repo:

  cp resources/claude/goal-orchestrator/{SKILL,SPEC,IMPL}.md \\
     ~/dev/agent-orchestration-skills/skills/goal-orchestrator/

(or the reverse, if the skills repo holds the newer text) — and diff before overwriting:
the copies have diverged in BOTH directions before.
EOF
exit 1
