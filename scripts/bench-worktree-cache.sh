#!/usr/bin/env bash
# Measure the worker's shared build cache: cold vs warm second-task build of a Rust workspace.
#
#   scripts/bench-worktree-cache.sh <hyperpanes-binary> [crate-dir] [base]
#     crate-dir  cargo workspace inside the repo to build   (default rs/crates/app)
#     base       committish the task worktrees fork from    (default HEAD)
#
# Drives the REAL runner (`<binary> worker --worktree`) through two sequential tasks on a scratch
# queue of the control API named by $HYPERPANES_CONTROL_FILE (default the live one). The runner is
# only a client: nothing here boots an app instance. Each task does `cargo build --locked` in its
# own fresh worktree (a different path every time) with CARGO_TARGET_DIR in the task's cache slot.
# Task 1 starts from an empty slot (cold). Task 2 reuses slot-0 (warm).
# Results go to $OUT/results.txt, and each build's log sits next to it.
#
# Env: BENCH_ROOT (worktree root, default ~/.cache/hp-wtdisk-bench/root; wiped first),
#      BENCH_QUEUE (default hyperpanes-wtdisk-bench).
set -euo pipefail

BIN="$(realpath "${1:?usage: $0 <hyperpanes-binary> [crate-dir] [base]}")"
CRATE="${2:-rs/crates/app}"
BASE="${3:-HEAD}"
REPO="$(git rev-parse --show-toplevel)"
OUT="${BENCH_OUT:-$HOME/.cache/hp-wtdisk-bench}"
ROOT="${BENCH_ROOT:-$OUT/root}"
QUEUE="${BENCH_QUEUE:-hyperpanes-wtdisk-bench}"
CF="${HYPERPANES_CONTROL_FILE:-$HOME/.local/state/hyperpanes/control.json}"

case "$ROOT" in /tmp/*) echo "refusing a /tmp root (tmpfs): $ROOT" >&2; exit 2;; esac
rm -rf "$ROOT"; mkdir -p "$OUT"; : > "$OUT/results.txt"

port=$(python3 -c "import json,sys;print(json.load(open(sys.argv[1]))['port'])" "$CF")
tok=$(python3 -c "import json,sys;print(json.load(open(sys.argv[1]))['token'])" "$CF")
api() { curl -fsS -H "Authorization: Bearer $tok" -H 'Content-Type: application/json' "http://127.0.0.1:$port$1" "${@:2}"; }

for n in 1 2; do
  api "/queues/$QUEUE/tasks" -d "{\"title\":\"bench build $n\",\"payload\":\"$n\",\"maxAttempts\":1,\"visibilityTimeoutMs\":3600000}" >/dev/null
done

# One worker, so the two tasks run back to back and task 2 lands on the slot task 1 warmed.
child='set -u
cd "$HP_WORKTREE/'"$CRATE"'"
export CARGO_TARGET_DIR="$HP_WORKTREE_CACHE_DIR/cargo-target"
log="'"$OUT"'/build-$HP_TASK_PAYLOAD.log"
s=$(date +%s.%N); cargo build --locked >"$log" 2>&1; rc=$?; e=$(date +%s.%N)
printf "task=%s slot=%s worktree=%s wall=%.1fs rc=%s compiled=%s\n" "$HP_TASK_PAYLOAD" \
  "$(basename "$HP_WORKTREE_CACHE_DIR")" "$HP_WORKTREE" "$(echo "$e - $s" | bc)" "$rc" \
  "$(grep -c "^ *Compiling" "$log")" >> "'"$OUT"'/results.txt"
exit $rc'

cd "$REPO"
HYPERPANES_CONTROL_FILE="$CF" "$BIN" worker --queue "$QUEUE" --worktree --base "$BASE" \
  --worktree-root "$ROOT" --min-free 0 -- sh -c "$child"

cat "$OUT/results.txt"
echo "warm build recompiled (task 2):"; grep "^ *Compiling" "$OUT/build-2.log" | sed 's/^ */  /'
