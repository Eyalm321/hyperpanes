# Worker worktree fork point: `--base` (required with `--worktree`)

`hyperpanes worker --worktree` creates one throwaway git worktree per claimed task. This doc is
the single source of truth for where those worktrees fork from; SKILL.md / SPEC.md link here
rather than restating it.

## The old behaviour (the defect)

Until v0.0.27, the runner passed a literal `HEAD` to `git worktree add`
(`rs/crates/app/src/worker.rs`), in **two** places:

- the `git worktree add … HEAD` fork point itself, and
- the stale-task-branch guard (`git merge-base --is-ancestor <branch> HEAD`), which decides
  whether a leftover `worker/<queue>/<id8>` branch is "empty, safe to reset" — judged against
  the wrong baseline whenever HEAD wasn't the intended base.

So the fork point was *whatever the runner cwd's checkout happened to have checked out* — a
docs-only branch, another agent's in-progress state, anything. In a shared checkout driven by
multiple agents this silently forked impl work from the wrong commit; "always point cwd at the
right checkout" was a pure-discipline rule, and it failed exactly as often as an agent forgot it.

## The new behaviour

- `--worktree` now **requires** `--base <committish>` (any branch, tag, or sha). Omitting it is a
  parse error whose message shows both real forms: `--base main` for independent work,
  `--base <your-integration-branch>` for a dependent wave.
- The base is validated up front (must resolve to a commit in the runner's cwd), so a typo fails
  before any task is claimed.
- Both HEAD uses are fixed: the worktree forks from the base, and the stale-branch guard compares
  against the base.
- The ref is resolved **per task claim**, not pinned at runner start: a dependent wave can point
  `--base` at the goal's integration branch and each task forks from that branch's *current* tip,
  which is the whole reason agents used to park work in a shared checkout.
- `--base HEAD` remains expressible for anyone who genuinely wants the checkout's current HEAD —
  the defect was implicitness, not HEAD itself.

## Migration

- Bare runner invocations: add the flag —
  `hyperpanes worker --queue <q> --count N --worktree --base main -- …`.
- Dependent waves: `--base <goal-integration-branch>` instead of checking that branch out in a
  shared cwd.
- **Version-skew gap:** a hyperpanes binary with this change plus a published `hyperpanes-mcp`
  *without* the `base` passthrough (≤ 0.1.12) means `spawn_workers {isolation:"worktree"}` spawns
  runners that exit immediately with the teaching error — loud, not silent. Until the MCP
  follow-up below is published, either spawn worker panes running the bare runner command
  (`open_pane`), or point `HYPERPANES_WORKER_BIN` at a wrapper script that injects `--base`.

## Follow-up: `base` passthrough in hyperpanes-mcp (apply after the pending MCP branches land)

`~/dev/hyperpanes-mcp/src/control-tools.ts` currently has unmerged branches rewriting the
`spawn_workers` block (`g1/talk`, `g2/spawn-workers-passthrough`), so this change is deliberately
**not** on a branch — apply it against whatever lands, in the `spawn_workers` registration:

1. Schema, next to `isolation`:
   ```ts
   base: z
     .string()
     .optional()
     .describe('fork point committish for isolation:"worktree" task worktrees (REQUIRED with it): "main" for independent work, the goal\'s integration branch for a dependent wave'),
   ```
2. Handler guard (fail fast in the tool instead of a worker pane dying after the call already
   returned ok): `if (isolation === 'worktree' && !base) throw new Error('isolation:"worktree" requires base — the fork point must be explicit (e.g. base:"main", or the goal\'s integration branch for a dependent wave)');`
3. In `argsFor`, next to the `--worktree` element: `...(base ? ['--base', base] : [])`.
4. Test (spawn-workers.test.ts): worktree+base yields `--base <value>` in the runner argv;
   worktree without base rejects with an error naming `base`.
