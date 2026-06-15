# Auto-merge: branch protection + merge settings (apply in the GitHub UI)

This repo's unattended-merge pipeline is built in phases. Phase 0 landed the `verify` gate
and the CODEOWNERS backstop. **Phase 6 (this document now covers it) adds the two guardrail
checks that make agent-PR self-merge SAFE** — `merge-guard` (policy/rate guardrails) and
`llm-review` (qualitative gate) — and documents the full gate chain plus the exact
branch-protection settings.

The gate chain a green agent PR must clear, in order of trust boundary:

```
                 ┌──────────────── required status checks (the trust boundary) ───────────────┐
agent opens PR → │  verify  →  merge-guard  →  llm-review  │ → merge queue → SQUASH merge to main
   (label agent) │  (test/   (kill switch,   (LLM judge,    │   (rebase, re-run
                 │  lint/     agent label,    fail-closed)   │    all 3 checks,
                 │  build)    protected paths,               │    serialize, throttle)
                 │            diff cap, hourly throttle)      │
                 └────────────────────────────────────────────┘
```

All three are independent **required status checks** — there is no human in the green path.
The only human stop is CODEOWNERS review on risky paths (§3), which the bot cannot satisfy.

> Phase 0 deliberately stopped short of turning auto-merge **on** and of requiring
> `llm-review` / `merge-guard` (they didn't exist yet). Phase 6 adds both workflows, so they
> can now be made **required** and auto-merge flipped on (start low — see §7 rollout).

All settings below are applied **by hand in the GitHub web UI** — this doc does not call
the GitHub API or `gh`. Repo: `Eyalm321/hyperpanes`, default branch **`main`**.

---

## 0. What exists after Phase 6

| Artifact | Path | Role |
|---|---|---|
| Verify gate | `.github/workflows/verify.yml` | Umbrella required check named **`verify`** = test (3 OS × 2 workspaces) + blocking fmt/clippy + conditional GUI build |
| Merge guard | `.github/workflows/merge-guard.yml` | Required check named **`merge-guard`** = kill switch + agent label + protected-path block + diff-size cap + hourly throttle (git-log based, no API) |
| LLM review | `.github/workflows/llm-review.yml` | Required check named **`llm-review`** = qualitative LLM judge, fail-closed; calls `scripts/llm-review.sh` |
| LLM judge script | `scripts/llm-review.sh` | The review step; reads diff+meta, writes `verdict.json` (`{verdict,risk,summary}`). Contains a clear `TODO(real-model-call)` for the Anthropic call; plumbing + pass/fail contract are real |
| Code owners | `.github/CODEOWNERS` | Forces human review on risky paths (CI/CD + the gates themselves, GUI crate, `build.rs`, deps, **auth/token + control server + session/PTY + secret redactor + the LLM-judge script**) |
| Legacy gate | `.github/workflows/test.yml` | Unchanged; report-only clippy/fmt for everyday human pushes |

The required status-check contexts are exactly three umbrella jobs — **`verify`**,
**`merge-guard`**, **`llm-review`** — each `needs:`-aggregating its own legs. Because matrix
jobs produce per-leg contexts like `test (rs, ubuntu-latest)`, requiring the umbrella job
(instead of enumerating every leg) means adding a future gate is a one-line `needs:` edit
with **no ruleset change**.

---

## 1. Repository-level merge settings

**Settings → General → "Pull Requests"**:

- ✅ **Allow squash merging** — and set the default commit message to *"Pull request title"*.
- ⬜ Allow merge commits — **off**.
- ⬜ Allow rebase merging — **off**.
- ✅ **Automatically delete head branches**.
- ✅ **Allow auto-merge** *(safe to enable now; nothing self-merges until branch protection
  + auto-merge are armed in a later phase — it just makes the "Enable auto-merge" button
  available)*.

Squash + delete-branch gives one commit per task on trunk → trivial `git revert`, clean
`git bisect`, readable history when many agents land per day.

---

## 2. Branch protection on `main`

Use a **repository ruleset** (Settings → **Rules → Rulesets → New branch ruleset**).
Rulesets supersede classic branch protection and are the forward-looking option.

- **Name**: `trunk-automerge`
- **Enforcement status**: **Active**
- **Target branches**: Add target → **Include default branch** (resolves to `main`).

Enable these rules:

### Require a pull request before merging
- **Required approvals: `0`** — checks are the reviewer, not humans (see §4).
- ✅ **Require review from Code Owners** — this is what makes `.github/CODEOWNERS` bite:
  risky paths still need a real human owner's approval. A bot approval cannot satisfy a
  code-owner requirement.
- ✅ **Dismiss stale pull request approvals when new commits are pushed**.
- ✅ **Require conversation resolution before merging**.

### Require status checks to pass
- ✅ **Require branches to be up to date before merging** (strict).
- **Required checks** — add all three (this is the entire trust boundary for unattended merge):
  - `verify`
  - `merge-guard`
  - `llm-review`
- Tip: a check only appears in the search box after it has run at least once, so open a
  throwaway PR (or push a branch) to trigger all three, then add them here. `merge-guard`
  needs `vars.AUTOMERGE_ENABLED=true` to pass (see §4); set it before requiring the check or
  every PR blocks.

### Other rules
- ✅ **Require linear history**.
- ✅ **Block force pushes** (the "Non-fast-forward" / restrict-force-push rule).
- ✅ **Restrict deletions**.
- ✅ **Require signed commits** *(recommended — the automation bot signs its commits)*.
- ✅ **Require merge queue** *(recommended; can be deferred past Phase 0)* — when enabled,
  set: merge method **Squash**, build concurrency to taste, grouping **ALLGREEN**. The
  queue rebases each PR onto latest trunk and re-runs the required checks against that
  rebased branch — this is why `verify.yml` also triggers on `merge_group`. It eliminates
  the "two PRs each green against stale `main` but broken together" race.

### Bypass list
- Keep the **bypass list empty** so the rules apply to everyone, including admins
  ("Include administrators"). A stray human push can't bypass the gate either.

---

## 3. CODEOWNERS behavior (the hard backstop)

With "Require review from Code Owners" on, any PR touching a path in `.github/CODEOWNERS`
needs that owner's approval. Owned (human-review-required) paths in this repo:

```
/.github/                              # CI/CD, the gates THEMSELVES (incl. merge-guard,
                                       #   llm-review), secrets usage, this file
/scripts/llm-review.sh                 # the LLM-judge script — editing it could fail-open
                                       #   the qualitative gate, so it is human-gated too
/rs/crates/app/                        # GPU/GUI surface (not exercised by the per-PR test gate)
/rs/crates/core/src/control/tokens.rs  # auth: minting/validating control-plane tokens
/rs/crates/core/src/control/server.rs  # network-exposed control plane
/rs/crates/core/src/control/routes.rs  # control-plane routing
/rs/crates/core/src/control/scope.rs   # control-plane authorization boundary
/rs/crates/core/src/session/           # PTY spawn + daemon transport (process exec surface)
/rs/crates/core/src/ai/redactor.rs     # secret redaction on the AI/telemetry path
**/build.rs                            # build-script = arbitrary code execution
**/Cargo.toml                          # dependency / supply-chain
**/Cargo.lock
```

These are the genuinely risky paths: anything that runs code at build time, exposes or
authorizes the control plane, mints/validates tokens, spawns processes, redacts secrets, or
touches the gates that protect all of the above. Everything else has no owner → it flows
through the checks-only path. CODEOWNERS is path-scoped, so only the dangerous minority stops
for a human, and it is the **authoritative** stop — `merge-guard`'s protected-path check (§4)
mirrors these globs only so the PR is *obviously* parked rather than silently stuck.

---

## 4. Why approvals = 0 + code-owner review

Setting required approvals to `0` while requiring **code-owner** review (plus required
status checks) means:

- Safe PRs (no owned paths) merge on **green checks alone** — no human in the loop.
- Risky PRs (owned paths) still require a real human owner's approval, which a bot
  identity cannot provide. This keeps CODEOWNERS semantically meaningful.

The qualitative reviewer (`llm-review`, §6) is wired exactly this way — as a **required
status check**, *not* a PR approval — so a failing judgment holds the merge without consuming
the human "required review" slot reserved for code owners on risky paths.

---

## 4. `merge-guard` — the policy / rate guardrails (`.github/workflows/merge-guard.yml`)

Required status check **`merge-guard`**. It is the quantitative trust boundary: GitHub has no
native "is this an agent PR?", "how big?", or "N merges/hour?" knobs, so this check supplies
them. Any guardrail tripping fails the check with a clear `::error::` message; because the
check is *required*, the PR simply sits "pending auto-merge" until it passes (backpressure,
not a hard rejection — except where a human is genuinely needed).

Guardrails, in order:

1. **Kill switch** — `vars.AUTOMERGE_ENABLED` must equal `"true"`. Flip it to `false` and
   **every** pending auto-merge freezes within one check cycle. No code deploy. This is the
   "everything on fire" stop; set it from a post-merge red alert or by hand.
2. **Agent label** — the PR must carry the `agent` label. That label is the switch the whole
   pipeline keys on; human PRs never carry it and so are never auto-merged.
3. **Protected paths** — the diff must touch none of the CODEOWNERS-owned globs (§3). The
   step reads `.github/CODEOWNERS` at runtime and translates its globs to match the changed
   files, so the two can't drift. CODEOWNERS in branch protection is the authoritative stop;
   failing here makes the PR obviously parked instead of silently stuck.
4. **Diff-size cap** — total changed lines (`git diff --numstat`) must be under
   `vars.AUTOMERGE_MAX_DIFF_LINES` (**default 400**). An oversized unattended diff is harder
   for the judge to vet and has a larger blast radius → send it to a human.
5. **Hourly throttle (no API)** — counts commits on the base-branch tip within the trailing
   hour from **`git log` timestamps** (`--since='1 hour ago'`). With squash + linear history
   (§1) that is one commit per merged PR, an accurate proxy for "auto-merges this hour". When
   it reaches `vars.AUTOMERGE_PER_HOUR` (**default 6**) the check fails → the PR *waits*; it
   re-runs and drains as the window slides. This bounds how fast a systematic bad pattern can
   flood trunk before a human notices. No `gh`/API call is made anywhere in this workflow.

Inside the **merge queue** (`merge_group` event) there is no PR payload (no labels, no PR
base diff); the queue only holds PRs that already cleared `merge-guard` at PR time, and the
queue's own `max_entries_to_merge` is a second structural throttle, so the `merge_group` run
asserts only the kill switch and passes the PR-scoped guardrails.

Repo variables this check reads (**Settings → Secrets and variables → Actions → Variables**):

| Variable | Default | Meaning |
|---|---|---|
| `AUTOMERGE_ENABLED` | *(unset → blocks)* | Master kill switch; must be `true` to allow any auto-merge |
| `AUTOMERGE_PER_HOUR` | `6` | Max auto-merges counted in the trailing hour |
| `AUTOMERGE_MAX_DIFF_LINES` | `400` | Max total changed lines for an unattended diff |

Self-defense: triggers are `pull_request` + `merge_group` (base-repo context). The workflow
never checks out or runs PR head code — diffs/labels/paths are read as data — so a malicious
PR cannot disable its own guardrails.

---

## 5. Repo variables to set before requiring the checks

In **Settings → Secrets and variables → Actions**:

- **Variables**: `AUTOMERGE_ENABLED` (set `false` until rollout step §7.5, then `true`),
  optionally `AUTOMERGE_PER_HOUR`, `AUTOMERGE_MAX_DIFF_LINES`, `LLM_REVIEW_MODEL`.
- **Secrets**: `ANTHROPIC_API_KEY` — consumed only by `llm-review` (and only in the base-repo
  context, never exposed to PR head code).

> Order matters: a *required* `merge-guard` with `AUTOMERGE_ENABLED` unset will block every
> PR (kill switch fails closed). Either set the variable first, or add the required check only
> at rollout step §7.

---

## 6. `llm-review` — the qualitative gate (`.github/workflows/llm-review.yml`)

Required status check **`llm-review`**. This is the reviewer that test/lint/build cannot be:
"does this diff actually do what the issue asked? is it a plausible-but-wrong fix? does it
delete a test, add a backdoor, or weaken auth/token handling?"

Contract (real and enforced by the workflow):

- The workflow collects `diff.patch` (capped at 120 KB) + `meta.json` (`{title, body}`) and
  runs `scripts/llm-review.sh`, passing the model id and the diff/meta/out paths via env.
- `scripts/llm-review.sh` MUST write `verdict.json`:
  `{ "verdict": "approve" | "block", "risk": <0-10>, "summary": "<text>" }`.
- The workflow's *Enforce verdict* step parses it: `approve` → exit 0 (check passes);
  anything else, or a missing/unparseable `verdict.json` → exit 1 (**fail-closed** — no
  merge). An API error/timeout must never fail-open.
- The verdict is uploaded as a workflow artifact (`llm-review-verdict`) and echoed into the
  job summary, so every unattended merge has an auditable verdict trail.

`scripts/llm-review.sh` ships with the gate **policy/rubric** real and a clearly marked
`TODO(real-model-call)` where the Anthropic API call goes (a reference Python snippet is
inline). Until that TODO is implemented the script emits a conservative **`block`** verdict,
so the gate is wired and fail-closed out of the box — it cannot accidentally approve before
the real judge exists. Implement the call (and `ANTHROPIC_API_KEY`) to enable approvals.

Notes:
- It is a **status check, not a PR approval** — a failing judgment holds the merge without
  consuming the code-owner review slot (§4 of the original rationale).
- It runs on `pull_request` / `merge_group` (base-repo context) and reads the diff as data,
  never executing PR code, so a malicious PR can't rewrite the rubric or read the API key.
- Model routing: default `vars.LLM_REVIEW_MODEL` (a sonnet-class id); escalate for
  large/security-critical diffs, downgrade for trivial ones, inside the script. Model ids
  move — verify against the current Anthropic model list before shipping.

---

## 7. Rollout + Phase-6 acceptance checklist

Lowest-risk order to turn the guardrails on:

1. **Land the workflows** (`merge-guard.yml`, `llm-review.yml`, `scripts/llm-review.sh`) +
   the expanded `CODEOWNERS`. Don't require the new checks yet.
2. **Set repo variables** (§5): `AUTOMERGE_ENABLED=false` first, plus
   `AUTOMERGE_PER_HOUR` / `AUTOMERGE_MAX_DIFF_LINES` if overriding defaults; add the
   `ANTHROPIC_API_KEY` secret.
3. **Validate `merge-guard` on a throwaway agent-labelled PR**: with `AUTOMERGE_ENABLED=false`
   it fails on the kill switch; flip to `true` and confirm a small, non-risky, `agent`-labelled
   PR passes, an unlabelled one fails, one touching `rs/crates/core/src/control/tokens.rs`
   fails the protected-path guardrail, and a 500-line diff fails the size cap.
4. **Validate `llm-review`**: until the `TODO(real-model-call)` is implemented it emits
   `block` (fail-closed) — confirm the check fails and `verdict.json` is uploaded. Implement
   the model call, then tune the rubric until its block rate on known-good human PRs is ~0.
5. **Require all three checks** in the ruleset (§2): `verify`, `merge-guard`, `llm-review`.
   Set `AUTOMERGE_ENABLED=true` and start with `AUTOMERGE_PER_HOUR=3`; let a handful of agent
   PRs self-merge and watch trunk. Raise the budget as confidence grows.

Phase-6 acceptance checklist:

1. `merge-guard.yml`, `llm-review.yml`, `scripts/llm-review.sh` merged to `main`; CODEOWNERS
   expanded to the risky paths in §3.
2. Repo variables/secrets per §5 are set.
3. Ruleset `trunk-automerge` requires **`verify` + `merge-guard` + `llm-review`**, with
   0 approvals, code-owner review on, linear history, force-push blocked, admins included.
4. Open a small `agent`-labelled PR touching no risky path → all three checks run; with
   `AUTOMERGE_ENABLED=true` and the judge implemented, it self-merges on green.
5. Open a PR touching e.g. `rs/crates/core/src/control/tokens.rs` → confirm it is **blocked**
   by both `merge-guard` (protected-path guardrail) and the CODEOWNERS human-review backstop.
6. Flip `AUTOMERGE_ENABLED=false` → confirm every pending auto-merge freezes within one check
   cycle (kill switch).
