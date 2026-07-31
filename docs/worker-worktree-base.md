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

## Worktree-create failure now nacks the task (no silent isolation fallback)

Same degradation family, second instance: when `git worktree add` failed (stale
`worker/<queue>/<id8>` branch with uncollected commits, git lock, disk), the runner used to log
one line and then run the task anyway **in its own cwd** — and ack it on success. The isolation
`--worktree` asked for silently became no isolation, with an unattended `claude
--dangerously-skip-permissions` sitting in the shared checkout.

Now:

- The task is **nacked**, with `worktree create failed (base <base>): <git error>` as the
  queue-visible reason; the child is never spawned. The runner prints the reason plus an explicit
  `child NOT run — refusing to execute without the requested isolation`, and the error names the
  branch and path it tried.
- The nack is the **standard retryable** one — `--nack-delay` backoff applies and the queue's
  `max_attempts` bounds it — because these failures are usually environmental and recoverable
  (collect or delete the stale branch and the retry succeeds); a persistent one dead-letters
  instead of spinning.
- The runner keeps draining: one task's broken worktree doesn't take the worker down or block the
  rest of the queue.

## Migration

- Bare runner invocations: add the flag —
  `hyperpanes worker --queue <q> --count N --worktree --base main -- …`.
- Dependent waves: `--base <goal-integration-branch>` instead of checking that branch out in a
  shared cwd.
- **Version-skew gap (closes when `g7/mcp-base` merges + publishes):** a hyperpanes binary with
  this change plus a published `hyperpanes-mcp` *without* the `base` passthrough (≤ 0.1.12) means
  `spawn_workers {isolation:"worktree"}` spawns runners that exit immediately with the teaching
  error — loud, not silent. Until the implemented follow-up below lands in a published package,
  either spawn worker panes running the bare runner command (`open_pane`), or point
  `HYPERPANES_WORKER_BIN` at a wrapper script that injects `--base`.

## Follow-up: `base` passthrough in hyperpanes-mcp — DONE on `g7/mcp-base`, pending merge + publish

**Do not re-implement this.** Goal g7 completed it on branch `g7/mcp-base` in
`~/dev/hyperpanes-mcp` (stacked on `g2/spawn-workers-passthrough`): `base` added to the
`spawn_workers` schema, REQUIRED with `isolation:"worktree"`, guarded in the handler before any
`newPane` is issued, emitted as `--base` in the runner argv; verified 170 tests green + tsc clean
in a clean checkout. What remains is merging g2+g7 and publishing the package — the version-skew
gap above applies only until then.

The spec as originally written is kept below as the historical record of what was asked
(`g7/mcp-base` is its implementation):

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
