# Auto-merge: branch protection + merge settings (apply in the GitHub UI)

This repo's unattended-merge pipeline is built in phases. **This document covers Phase 0
only**: landing the `verify` gate (`.github/workflows/verify.yml`) and the human-review
backstop (`.github/CODEOWNERS`), then wiring branch protection so `verify` is *required*.

> Phase 0 deliberately stops short of turning auto-merge **on**. The goal of this phase is
> to prove the `verify` gate is solid on ordinary human PRs first. Later phases add the
> `llm-review` and `merge-guard` required checks and flip auto-merge on — do **not** add
> those two as required checks yet (they don't exist, so every PR would block forever).

All settings below are applied **by hand in the GitHub web UI** — this doc does not call
the GitHub API or `gh`. Repo: `Eyalm321/hyperpanes`, default branch **`main`**.

---

## 0. What exists after Phase 0

| Artifact | Path | Role |
|---|---|---|
| Verify gate | `.github/workflows/verify.yml` | Umbrella required check named **`verify`** = test (3 OS × 2 workspaces) + blocking fmt/clippy + conditional GUI build |
| Code owners | `.github/CODEOWNERS` | Forces human review on risky paths (`.github/`, `rs/crates/app/`, `build.rs`, `Cargo.toml`/`Cargo.lock`) |
| Legacy gate | `.github/workflows/test.yml` | Unchanged; report-only clippy/fmt for everyday human pushes |

The single required status-check context is **`verify`** — the umbrella job that `needs:`
all the matrix legs. Because matrix jobs produce per-leg contexts like
`test (rs, ubuntu-latest)`, requiring the umbrella `verify` (instead of enumerating every
leg) means adding a future gate is a one-line `needs:` edit with **no ruleset change**.

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
- **Required checks** — add exactly one for Phase 0:
  - `verify`
  - *(Later phases add `llm-review` and `merge-guard`. Do not add them now.)*
- Tip: the check only appears in the search box after it has run at least once, so open a
  throwaway PR (or push a branch) to trigger `verify`, then add it here.

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
needs that owner's approval. Owned paths in this repo:

```
/.github/        # CI/CD, the gates, secrets, this file
/rs/crates/app/  # GPU/GUI surface (not exercised by the per-PR test gate)
**/build.rs      # build-script = arbitrary code execution
**/Cargo.toml    # dependency / supply-chain
**/Cargo.lock
```

Everything else has no owner → it flows through the checks-only path. CODEOWNERS is
path-scoped, so only the dangerous minority stops for a human.

---

## 4. Why approvals = 0 + code-owner review

Setting required approvals to `0` while requiring **code-owner** review (plus required
status checks) means:

- Safe PRs (no owned paths) merge on **green checks alone** — no human in the loop.
- Risky PRs (owned paths) still require a real human owner's approval, which a bot
  identity cannot provide. This keeps CODEOWNERS semantically meaningful.

When the later-phase qualitative reviewer arrives, wire it as a **required status check**
(`llm-review`), *not* as a PR approval — a failing check holds the merge without consuming
the human "required review" slot reserved for code owners on risky paths.

---

## 5. Phase-0 acceptance checklist

1. `verify.yml` + `CODEOWNERS` merged to `main`.
2. Repo merge settings per §1 (squash-only, delete branch on merge).
3. Ruleset `trunk-automerge` active on `main` with **`verify` required**, 0 approvals,
   code-owner review on, linear history, force-push blocked, admins included.
4. Open a normal PR → confirm `verify` runs and is required.
5. Open a PR touching e.g. `rs/crates/app/**` → confirm it is **blocked** pending a
   code-owner review (proves the backstop).

Once `verify` has been green and stable on human PRs, proceed to the next phase
(`merge-guard` + kill switch, then `llm-review`, then flip auto-merge on at a low rate).
