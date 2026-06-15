#!/usr/bin/env bash
#
# scripts/llm-review.sh — the LLM-judge step for the unattended auto-merge pipeline.
#
# Called by .github/workflows/llm-review.yml. Reads a PR diff + metadata, asks an LLM whether
# the change is safe to merge unattended, and writes a machine-readable verdict that the
# workflow turns into the pass/fail of the required `llm-review` status check.
#
# CONTRACT (this is real and the workflow depends on it — do not change the shape):
#   inputs  (env, with defaults):
#     LLM_REVIEW_DIFF   path to the (capped) unified diff           [default: diff.patch]
#     LLM_REVIEW_META   path to JSON {title, body}                  [default: meta.json]
#     LLM_REVIEW_OUT    path to write the verdict JSON              [default: verdict.json]
#     LLM_REVIEW_MODEL  model id to use                             [default: see below]
#     ANTHROPIC_API_KEY API key for the real model call             [required for the real call]
#   output (LLM_REVIEW_OUT), exactly:
#     { "verdict": "approve" | "block", "risk": <int 0-10>, "summary": "<terse text>" }
#   exit code:
#     0 always on a *successfully produced* verdict (approve OR block — the workflow decides
#       the gate from the verdict field, so a "block" verdict is still a successful run).
#     non-zero ONLY on an internal failure to produce a verdict (the workflow then fails
#       closed). This keeps the gate fail-closed: no verdict => no merge.
#
# The rubric below is the actual gate policy. Bias toward BLOCK when uncertain — a human will
# pick it up. The judge reads the diff as DATA; it never executes PR code.

set -euo pipefail

DIFF_PATH="${LLM_REVIEW_DIFF:-diff.patch}"
META_PATH="${LLM_REVIEW_META:-meta.json}"
OUT_PATH="${LLM_REVIEW_OUT:-verdict.json}"
# Cost/quality sweet spot for a per-PR gate; override via vars.LLM_REVIEW_MODEL.
# (Model ids move — verify against the current Anthropic model list before shipping.)
MODEL="${LLM_REVIEW_MODEL:-claude-sonnet-4-6}"

# --- input validation (fail-closed: a missing diff means we cannot judge) ---------------
if [ ! -f "$DIFF_PATH" ]; then
  echo "llm-review: diff '$DIFF_PATH' not found — cannot produce a verdict" >&2
  exit 1
fi
if [ ! -f "$META_PATH" ]; then
  echo "llm-review: meta '$META_PATH' not found — cannot produce a verdict" >&2
  exit 1
fi

PR_TITLE="$(jq -r '.title // ""' "$META_PATH")"
PR_BODY="$(jq -r '.body // ""' "$META_PATH")"

# The gate policy handed to the model. Identical every run -> prompt-cache it in the real call.
read -r -d '' RUBRIC <<'RUBRIC_EOF' || true
You are a strict release gatekeeper for an unattended merge pipeline.
APPROVE only if ALL of the following hold; otherwise BLOCK:
  1. The change plausibly accomplishes its stated title/issue.
  2. No tests deleted/skipped/weakened to make CI pass; no assertions gutted.
  3. No secrets committed, no obvious backdoor, no telemetry/exfiltration added.
  4. No weakening of security-critical code (auth, token minting/validation, the control
     server, signature/permission checks).
  5. Error handling is not silently swallowed; no panics introduced on the happy path.
  6. Scope matches the PR title (no unrelated drive-by edits).
Bias toward BLOCK when uncertain — a human will pick it up. Be terse.
Return ONLY JSON: {"verdict":"approve|block","risk":0-10,"summary":"..."}
RUBRIC_EOF

# ========================================================================================
# TODO(real-model-call): replace this block with the actual Anthropic API call.
# ----------------------------------------------------------------------------------------
# The plumbing around it (inputs, $OUT_PATH shape, exit-code contract, fail-closed) is REAL
# and MUST be preserved. Reference implementation for the call:
#
#   python3 - "$MODEL" "$DIFF_PATH" "$META_PATH" "$OUT_PATH" <<'PY'
#   import json, os, sys, anthropic
#   model, diff_path, meta_path, out_path = sys.argv[1:5]
#   meta = json.load(open(meta_path)); diff = open(diff_path).read()
#   client = anthropic.Anthropic(api_key=os.environ["ANTHROPIC_API_KEY"])
#   msg = client.messages.create(
#       model=model, max_tokens=1024,
#       system=[{"type":"text","text":os.environ["RUBRIC"],
#                "cache_control":{"type":"ephemeral"}}],   # cache the identical rubric
#       messages=[{"role":"user","content":
#           f"PR: {meta.get('title','')}\n\n{meta.get('body','')}\n\n=== DIFF ===\n{diff}"}])
#   text = msg.content[0].text
#   out = json.loads(text[text.find('{'):text.rfind('}')+1])
#   assert out.get("verdict") in ("approve","block")
#   json.dump(out, open(out_path,"w"))
#   PY
#
# Wiring notes for the real call:
#   * export RUBRIC so the heredoc above is visible to python (export RUBRIC).
#   * route the model by risk/cost: escalate $MODEL for large or security-critical diffs,
#     downgrade for trivial (docs/whitespace) diffs.
#   * FAIL-CLOSED: on API error / timeout / unparseable JSON, write a BLOCK verdict (below)
#     and exit 0 — the workflow will then hold the merge. NEVER emit an "approve" on error.
# ========================================================================================

# --- PLACEHOLDER verdict (until the real call replaces the block above) -----------------
# Conservative default: BLOCK with a clear note, so the gate is wired and fail-closed out of
# the box. Real deployments MUST implement the call above before relying on llm-review.
if [ -n "${ANTHROPIC_API_KEY:-}" ]; then
  PLACEHOLDER_SUMMARY="llm-review placeholder: ANTHROPIC_API_KEY is set but scripts/llm-review.sh has not yet implemented the real model call (see TODO). Failing closed — implement the call to enable approvals."
else
  PLACEHOLDER_SUMMARY="llm-review placeholder: no ANTHROPIC_API_KEY and no real model call implemented (see TODO). Failing closed by design."
fi

# Emit a well-formed verdict. jq guarantees valid JSON regardless of the summary text.
jq -n \
  --arg verdict "block" \
  --argjson risk 10 \
  --arg summary "$PLACEHOLDER_SUMMARY (model=$MODEL, title=$(printf '%.80s' "$PR_TITLE"))" \
  '{verdict:$verdict, risk:$risk, summary:$summary}' > "$OUT_PATH"

echo "llm-review: wrote $OUT_PATH (placeholder verdict=block; implement the TODO for real approvals)"

# A successfully produced verdict => exit 0. The workflow reads $OUT_PATH.verdict to gate.
exit 0
