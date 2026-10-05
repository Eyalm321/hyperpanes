#!/usr/bin/env bash
# setup-claude-accounts.sh — prepare multiple Claude accounts for hyperpanes goal-orchestrator
# account rotation, WITHOUT breaking `claude --resume` across accounts.
#
# WHY: the `claude` CLI stores conversation transcripts under $CLAUDE_CONFIG_DIR
# (…/projects/<cwd-hash>/<sessionId>.jsonl, plus …/sessions/). So a session created under
# one account is invisible to `--resume` when a pane runs a different CLAUDE_CONFIG_DIR.
# Goal-orchestrator rotates accounts per-pane and needs resume to keep working across a
# rotation — so transcripts must live in ONE shared store while credentials stay per-account.
#
# WHAT IT DOES (per account dir): move its projects/ + sessions/ into ~/.claude-shared,
# then replace them with symlinks back to the shared store. Credentials (.credentials.json)
# and everything else stay per-account. Result:
#
#   ~/.claude-shared/{projects,sessions}          # single real transcript store
#   ~/.claude       creds A ; projects,sessions -> shared
#   ~/.claude-alt   creds B ; projects,sessions -> shared
#   ~/.claude-alt2  creds C ; projects,sessions -> shared   (created empty; log in separately)
#
# SAFETY: dry-run by default (prints what it WOULD do). Pass --apply to execute. Every moved
# dir is backed up first. Idempotent: a dir already symlinked to the shared store is skipped.
# It NEVER deletes a real transcript — on merge collisions the existing shared copy wins and
# the incoming file is left in the backup.

set -euo pipefail

SHARED="${CLAUDE_SHARED_DIR:-$HOME/.claude-shared}"
ACCOUNTS=("$HOME/.claude" "$HOME/.claude-alt" "$HOME/.claude-alt2")
LINKED=(projects sessions)   # the two transcript dirs to share
APPLY=0
STAMP="$(date +%Y%m%d-%H%M%S)"

for arg in "$@"; do
  case "$arg" in
    --apply) APPLY=1 ;;
    -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
    *) echo "unknown arg: $arg (use --apply or --help)"; exit 2 ;;
  esac
done

say()  { printf '%s\n' "$*"; }
run()  { if [ "$APPLY" = 1 ]; then say "  RUN : $*"; eval "$@"; else say "  PLAN: $*"; fi; }

if [ "$APPLY" = 1 ]; then
  say "=== APPLY MODE — this will move real transcript data ==="
  # A live `claude` writes into projects/ + sessions/ continuously; relocating them out from
  # under a running process can lose or misplace a transcript write. Refuse if any are running.
  if pgrep -x claude >/dev/null 2>&1 || pgrep -f '/claude ' >/dev/null 2>&1; then
    say "REFUSING: a 'claude' process is running. Quit ALL claude instances first —"
    say "  including this session, the hyperpanes app's agent panes, and ~/.claude/daemon —"
    say "  then re-run with --apply. (Check:  pgrep -af claude)"
    exit 1
  fi
else
  say "=== DRY RUN — nothing is changed. Re-run with --apply to execute. ==="
fi
say "shared store: $SHARED"
say

# 1. Ensure the shared store exists.
run "mkdir -p '$SHARED/projects' '$SHARED/sessions'"

# 2. For each account dir, relocate + symlink its transcript dirs.
for acct in "${ACCOUNTS[@]}"; do
  say "account: $acct"
  if [ ! -e "$acct" ]; then
    say "  (missing — creating empty account dir; log in later with CLAUDE_CONFIG_DIR='$acct' claude)"
    run "mkdir -p '$acct'"
  fi
  for sub in "${LINKED[@]}"; do
    src="$acct/$sub"
    if [ -L "$src" ]; then
      say "  $sub: already a symlink -> $(readlink "$src" 2>/dev/null || echo '?') — skip"
      continue
    fi
    if [ -d "$src" ]; then
      # Back up, then merge contents into the shared store (existing shared file wins),
      # then replace the real dir with a symlink.
      bak="$acct/$sub.pre-share-$STAMP"
      say "  $sub: real dir — back up + merge into shared, then symlink"
      run "cp -a '$src' '$bak'"
      # -n = never overwrite an existing shared file (existing shared copy wins; leftovers stay in \$bak)
      run "cp -a -n '$src/.' '$SHARED/$sub/' 2>/dev/null || true"
      run "rm -rf '$src'"
      run "ln -s '$SHARED/$sub' '$src'"
    else
      say "  $sub: absent — symlink to shared"
      run "ln -s '$SHARED/$sub' '$src'"
    fi
  done
  say
done

if [ "$APPLY" = 1 ]; then say "Done (applied)."; else say "Done (dry run — nothing changed)."; fi
say "Verify with:  ls -la ${ACCOUNTS[0]}/projects ${ACCOUNTS[0]}/sessions"
say "Each account keeps its own .credentials.json. Confirm each is logged in:"
for acct in "${ACCOUNTS[@]}"; do
  say "  CLAUDE_CONFIG_DIR='$acct' claude   # then /login if needed"
done
