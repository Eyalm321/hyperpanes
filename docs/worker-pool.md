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
  shortens).
- Faster recovery paths exist beside the timer: `requeue_worker` (on a worker pane's exit)
  and `recover_in_flight` (on control-server boot — every worker is a dead child, so all
  in-flight claims are recovered).

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
| `POST /queues/{q}/claim` | **Claim.** Body: `{ worker, leaseMs?, count? }`. Returns `{ ok, tasks }` (0..count tasks) |
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
