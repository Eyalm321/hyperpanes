# Work Queue (Worker Pool)

A durable, claimable **work queue** built into the control plane. Where the per-pane
inbox is a fan-out *notification bus* (directed, at-least-once, lost on restart, no
ownership), the work queue is its inverse: a **competing-consumers** queue a controller
pane uses to hand tasks to a pool of headless worker panes. It adds exactly the three
properties the inbox lacks — **claim ownership**, **leases with fencing**, and
**durability** — backed by SQLite (WAL mode, crash-consistent).

Implementation: `rs/crates/core/src/control/work.rs` (pure rules + `WorkQueue` over
SQLite) and `rs/crates/core/src/control/routes.rs` (HTTP surface).

> The canonical surface is the **control API (HTTP)** documented below. The hyperpanes MCP
> server also wraps these routes as tools (`enqueue_task`, `list_tasks`, `claim_task`,
> `ack_task`, `nack_task`, `extend_task`, `get_task`, `list_queues`, `purge_queue`); raw
> HTTP with a bearer token works equivalently.

## Task lifecycle

```text
                claim (lease minted, fencingToken++, attempts++)
    Queued ───────────────────────────────────────────────▶ Claimed
      ▲  ▲                                                    │ │ │
      │  │ nack(requeue, attempts<max) / visibility reap      │ │ │ ack
      │  └────────────────────────────────────────────────────┘ │ ▼
      │            availableAt = now + backoff                   │ Done   (terminal)
      │                                                          │
      │  nack(requeue, attempts>=max) / reap-when-exhausted      ▼
      │                                                        Dead   (terminal, dead-letter)
      └─  nack(requeue=false) ───────────────────────────────▶ Failed (terminal, gave up)
```

| State     | Meaning                                                     | Terminal |
|-----------|-------------------------------------------------------------|----------|
| `queued`  | Available for claim once `availableAt <= now`               | no       |
| `claimed` | Leased to a worker, lease ticking                           | no       |
| `done`    | Worker `ack`ed success                                      | yes      |
| `failed`  | Worker/operator gave up (`nack` with `requeue:false`)       | yes      |
| `dead`    | Retries exhausted (the dead-letter)                         | yes      |

**Delivery is at-least-once.** A worker that finishes its side effect then dies *before*
`ack` has its lease reaped and the task re-run. **Handlers MUST be idempotent.**
Exactly-once is not offered; `dedupeKey` collapses duplicate *enqueues*, never duplicate
*executions*.

## Fencing tokens

Each claim mints a strictly-increasing `fencingToken`. This is what makes the visibility
timeout *safe* rather than merely convenient. The classic hazard:

1. Worker A claims a task (token 1) and stalls past its lease deadline.
2. The reaper requeues the task.
3. Worker B claims the requeued task → token 2, does the work.
4. Worker A wakes and tries to `ack`.

A still holds the *old* token 1, so its `ack`/`nack`/`extend` all return **409 Conflict**.
Only the holder of the current token may mutate the lease. The counter never regresses,
even across restart (resumed from `MAX(fencing_token)` on open), so a late worker can
never alias a live one.

`ack`/`nack`/`extend` therefore all **require** the `fencingToken` in the body — it is the
optimistic-concurrency check, not a convenience. A missing token is `400`; a wrong/stale
token is `409`.

## Lease / visibility timeout

On claim, the task gets a `visibilityDeadline = now + leaseMs`. A background reaper
(`reap_expired`) requeues any `claimed` task whose deadline has passed (or dead-letters it
if retries are exhausted) — recovering work from a worker that died silently. Requeue keeps
the now-stale token on the row, fencing the late worker out.

- `leaseMs <= 0` on claim falls back to the task's own `visibilityTimeoutMs` (default
  **30 000 ms**).
- Long-running workers heartbeat with `extend` (extends from the current deadline, never
  shortens). The `hyperpanes worker` runner asks for just enough to put the deadline back at
  `now + lease` each beat (half a lease apart), so its deadline stays within one lease of
  now. A dead runner's claim therefore expires within one lease, not "as long as the task
  had been running".
- Faster recovery paths exist beside the timer: `requeue_worker` (on a worker pane's exit)
  and `recover_in_flight` (on control-server boot: requeues every claim whose holder is
  dead, keeps live holders' claims; see [Across an app relaunch](#across-an-app-relaunch)).
  Boot recovery and the reaper run **only in the process that owns `work.db`** (an OS lock
  on `work.db.lock`). A second instance never attaches the DB, and `/health` reports its
  `workQueue` as `"refused"` (`docs/dev-instance-isolation.md`).

## Across an app relaunch

With the session daemon on, worker panes outlive the GUI: `restartApp {scope:"gui"}`, or the
GUI crashing or being closed while the daemon lives, leaves every `hyperpanes worker` runner
(and its `claude` child) running. The relaunched GUI then owns `work.db` and runs boot
recovery. Recovery must not requeue those claims (the surviving runner would be fenced out
with a 409 and the task would run twice), but it must still requeue every claim whose worker
really died.

**Holder identity.** A claim may name its holder process: `POST /queues/{q}/claim` takes an
optional `holderPid`. The runner sends its own pid. The server records it only if it is a live
**local** process, together with a process-start fingerprint (`<boot_id>:<starttime>` from
procfs on Linux, `ps -o lstart=` on other unix, none on Windows), in internal `work.db`
columns (`holder_pid`, `holder_start`, schema v3) that are not on the task wire. Every claim
overwrites them, so a re-claim never inherits a previous holder.

**Boot recovery rules** (`WorkQueue::recover_in_flight`), per `claimed` row:

| Holder | At boot | Covers |
|---|---|---|
| none recorded | requeue now (exhausted ⇒ dead-letter) | in-process claims, MCP `claim_task`, pre-v3 rows, remote claimers |
| recorded, dead: pid gone or zombie, or fingerprint mismatch (recycled pid, rebooted) | requeue now | `restartApp {scope:"full"}`, crash taking the daemon down, reboot |
| recorded, alive | stay `claimed`, same `fencingToken`; lease re-armed to `max(deadline, boot + visibilityTimeoutMs)` | `restartApp {scope:"gui"}`, GUI crash with the daemon alive |

The re-arm gives a surviving runner one full lease to reconnect even when the app was down
longer than its lease; it never shortens a lease.

**Worst-case recovery latency for a truly dead claim.** Zero (requeued at boot, claimable
immediately) whenever its holder can be judged dead, which is every case above on Linux and
macOS. The only exception is a holder whose pid has been recycled by an unrelated live process
on a platform without a fingerprint (Windows): recovery keeps the claim, and the reaper
requeues it once the re-armed lease expires: at most `max(remaining lease, visibilityTimeoutMs)
+ 5 s` (the reaper tick) after boot, i.e. **35 s** with the default 30 s visibility timeout and
a runner-held lease.

**The runner rides out the relaunch.** On a loopback bind the relaunched app listens on a fresh
port with a freshly minted token. The runner resolves its control file once at start
(`HYPERPANES_CONTROL_FILE`, else the default) and, on a connection error or a 401, re-reads
that same file and retries: claim/ack/nack within a 5-minute reconnect window, the heartbeat
every second until the child exits (it stops only on 409/404: a lost lease). Ack is idempotent
for the same fence, so retrying a lost ack is safe. If the app stays away past the window the
runner exits; its claim then has a dead holder, so the next boot requeues it.

**Trust.** `holderPid` is self-reported by the claimant (the server cannot see a loopback peer's
pid). It only ever affects the claim made by that same request, so a token holder can at most
keep *its own* claim alive across a relaunch (until the re-armed lease lapses with no
heartbeat); it cannot pin or release anyone else's claim, and a pid that is not a live local
process is ignored (no holder ⇒ legacy requeue).

**MCP holders.** Agents claiming through the `hyperpanes-mcp` bridge (`claim_task`) keep the
legacy behaviour (requeued at boot) until the bridge sends `holderPid`; Eyalm321/hyperpanes-mcp#9
makes it send its own pid (the bridge lives exactly as long as its agent session). The bridge
already re-reads `control.json` on every call, so it reconnects to a relaunched app on its own.

## maxAttempts, priority, dedupeKey

- **`maxAttempts`** (default **5**) — a task dead-letters once `attempts >= maxAttempts`.
  `attempts` increments on every claim. On `nack(requeue:true)`, an exhausted task goes to
  `dead` instead of back to `queued`.
- **Backoff** — a requeued task's `availableAt` is pushed to `now + backoff(attempts)`:
  exponential `1000ms * 2^(attempts-1)`, capped at **60 000 ms**. `nack`'s `delayMs`
  overrides the computed backoff.
- **`priority`** (default **0**) — claim order is `priority DESC`, then FIFO
  (`availableAt ASC`, `seq ASC`). Higher priority is claimed first; ties break by enqueue
  order.
- **`dedupeKey`** — collapses duplicate *enqueues* while a task with that key is still live
  (`queued`/`claimed`): the existing task is returned unchanged, no new row. The key frees
  once that task reaches a terminal state. (De-dupes enqueues, not executions.)

## Competing-consumer semantics

`claim` runs in a single `BEGIN IMMEDIATE` transaction: it atomically picks the best
claimable task and marks it `claimed`, so two concurrent claimers can never win the same
row. A pool of workers all poll the same `POST /queues/{q}/claim`; each task is handed to
exactly one worker. A drained queue returns `200 {"tasks":[]}` (never 204). `seq` is a
global monotonic id (shared across queues) that doubles as the `list` cursor.

## Control API routes

All routes require `Authorization: Bearer <token>` and pass a queue-scope gate (a scoped
token is restricted to its queues; a master token passes any). Bodies are JSON; tasks
serialize camelCase with epoch-ms timestamps.

| Method & path | Purpose |
|---|---|
| `GET /queues` | Every queue with at least one task + its depth-by-state (scope-filtered) |
| `POST /queues/{q}/tasks` | **Enqueue.** Body: `{ payload, kind?, title?, priority?, maxAttempts?, visibilityTimeoutMs?, availableAt?\|delayMs?, dedupeKey? }`. Returns `{ ok, id, seq }` |
| `GET /queues/{q}/tasks` | **List/inspect.** Query: `after` (seq cursor), `state`, `limit` (≤1000). Returns `{ queue, tasks, counts, latestSeq }` |
| `POST /queues/{q}/claim` | **Claim.** Body: `{ worker, leaseMs?, count?, holderPid? }` (`holderPid`: the claiming process, see [Across an app relaunch](#across-an-app-relaunch)). Returns `{ ok, tasks }` (0..count tasks) |
| `POST /queues/{q}/purge` | **Retention.** Drop terminal tasks. Body: `{ olderThan?\|olderThanMs? }` (none ⇒ all terminal). Returns `{ ok, removed }` |
| `GET /tasks/{id}` | Fetch one task (scope resolved from its queue) |
| `POST /tasks/{id}/ack` | **Complete.** Body: `{ fencingToken, result? }`. `claimed → done` |
| `POST /tasks/{id}/nack` | **Fail/retry.** Body: `{ fencingToken, requeue?, error?, delayMs? }`. `requeue` defaults `true` |
| `POST /tasks/{id}/extend` | **Heartbeat.** Body: `{ fencingToken, extraMs }`. Renews the lease |

`payload`, `kind`, `title` are **opaque** to the queue — application fields (repo, base,
prompt, branch, pr, …) ride inside `payload`. The queue owns only lifecycle + lease +
retry/backoff columns.

### Status codes

- `200` — success (including an empty claim).
- `400` — bad body (missing `payload`, missing `worker`, missing `fencingToken`, …).
- `401` — missing/invalid bearer token.
- `403` — queue out of scope.
- `404` — no such task.
- `409` — **stale lease** (wrong `fencingToken`, or task no longer in a leasable state).

## Worker loop sketch

```text
loop:
  POST /queues/build/claim   { "worker": "wkr-7", "leaseMs": 60000 }
    → 200 { tasks: [ { id, fencingToken, payload, ... } ] }
    → if tasks == [] : sleep, retry
  do the work (idempotently; heartbeat POST .../extend for long jobs)
  on success: POST /tasks/{id}/ack   { fencingToken, result }
  on failure: POST /tasks/{id}/nack  { fencingToken, requeue: true, error }
```

Re-`ack`ing an already-`done` task with the **same** token is idempotent success (covers a
lost-200 retry); any other token is `409`.

## Worktree isolation (`hyperpanes worker --worktree`)

The native runner can run each claimed task in a throwaway git worktree on a fresh
`worker/<queue>/<id8>` branch, forked from an explicit `--base <committish>` (required; see
[`worker-worktree-base.md`](worker-worktree-base.md)). Since the wtdisk change
([`worker-worktree-disk.md`](worker-worktree-disk.md)):

- **Location.** Worktrees live under a disk-backed root, `<cache dir>/hyperpanes/worktrees/<repo>-<hash8>/wt/`
  (`~/.cache` on Linux, `~/Library/Caches` on macOS, `%LOCALAPPDATA%` on Windows), not the
  system temp dir. Override it with `--worktree-root <dir>` or `HYPERPANES_WORKTREE_ROOT`, or
  through `spawn_workers`' `env`.
- **Cleanup.** A finished task's worktree is removed and its branch kept. Every worktree has an
  OS file lock held by its runner (and by the task's child on unix), so a runner that starts
  sweeps the orphans of crashed runners for its repo, then runs `git worktree prune`.
- **Build cache.** The child gets `HP_WORKTREE_CACHE_DIR`, a per-repo cache slot held
  exclusively for the task. Concurrent tasks get distinct slots, and a slot is reused warm by
  later tasks. The goal personas set `CARGO_TARGET_DIR=$HP_WORKTREE_CACHE_DIR/cargo-target`.
- **Disk guard.** Before creating a worktree, the runner checks free space on the root
  (`--min-free` / `HYPERPANES_WORKTREE_MIN_FREE`, default `10G`, `0` = off). If it's short, the
  task is nacked with an `insufficient disk space …` reason and a 60 s backoff, the child is not
  run, and the worker stops.

Child environment: `HP_TASK_ID`, `HP_TASK_PAYLOAD`, `HP_TASK_TITLE`, `HP_FENCING_TOKEN`,
`HP_QUEUE`, plus `HP_WORKTREE` and `HP_WORKTREE_CACHE_DIR` under `--worktree`.
