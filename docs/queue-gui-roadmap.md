# Queue GUI Roadmap

The durable work queue (`core::control::work::WorkQueue`, worker-pool phase 2/3) currently
has **no native GUI surface** — it is reachable only over the control API (HTTP) and, more
recently, the matching MCP tools (`list_queues`, `list_tasks`, `purge_queue`, …). This doc
sketches the planned in-app panel that exposes it, tracked as four GitHub issues that compose
into one **per-project task panel** in the sidebar.

Because the app embeds the same `core` crate that serves the control API, the GUI reads the
queue **in-process** through the exact route handlers below — no extra socket hop.

## Control-API data the panel reads

| Route | Returns | Used by |
| --- | --- | --- |
| `GET /queues` | `{ queues: [{ queue, counts }] }`, scope-filtered | #6, #7 |
| `GET /queues/{queue}/tasks` | `{ queue, tasks: [Task], counts }`; cursor `after`, optional `state`, `limit` | #7, #8 |
| `GET /tasks/{id}` | one `Task` | #8 |
| `POST /queues/{queue}/purge` | `{ removed }` — drops terminal tasks (`older_than` / `state` body, else all terminal) | #8 |
| `POST /tasks/{id}/nack` | `{ requeue:false }` ⇒ a claimed task → `Failed` (the "cancel" verb today) | #8 |
| `GET /events` (WebSocket) | server→client frames; no queue frame yet (see #9) | #9 |

`Counts` is depth-by-state: `{ queued, claimed, done, failed, dead }`. A `Task` carries
`id, queue, seq, kind, title, state, payload, priority, attempts, maxAttempts, availableAt`
plus the lease fields (`claimedBy, fencingToken, visibilityDeadline, result, error`).

## The four issues

### #6 — Per-project task-count badge (sidebar)

- **Shows:** a small count badge on each project row in the sidebar (e.g. queued + claimed
  depth, or total non-terminal work).
- **Where:** the existing project list rendered by `app::sidebar` (newest-first by
  `last_opened_at`, same order `projects::list_projects()` returns).
- **Reads:** `GET /queues` → sum the `counts` of the queue(s) mapped to that project.

### #7 — Per-project task state lanes

- **Shows:** a compact breakdown of the project's queue by state — Queued / Claimed / Done /
  Failed / Dead — as lanes or a segmented bar.
- **Where:** an expandable region under the project row, or the header of the task panel (#8).
- **Reads:** `counts` from `GET /queues` for the overview; `GET /queues/{queue}/tasks?state=…`
  to populate a lane on demand.

### #8 — Per-project task list + purge / cancel actions

- **Shows:** the actual tasks for the project (title, kind, state, attempts), with row actions.
- **Where:** a dedicated task panel opened from the project row / a lane in #7.
- **Reads:** `GET /queues/{queue}/tasks` (paged via the `seq` cursor `after`); `GET /tasks/{id}`
  for detail.
- **Actions:** **Purge** → `POST /queues/{queue}/purge` (terminal cleanup). **Cancel** → for a
  *claimed* task, `POST /tasks/{id}/nack {requeue:false}` (→ `Failed`). ⚠ There is no route to
  delete a still-`Queued` task today — a first-class cancel verb is a queue-side follow-up.

### #9 — Live per-project queue updates via a control event

- **Shows:** the badge (#6), lanes (#7) and list (#8) updating without polling.
- **Where:** the same surfaces, refreshed reactively.
- **Reads:** a **new** `ControlEvent` variant (e.g. `Queue` / `Task`) broadcast on
  enqueue / claim / ack / nack / reap. `core::control::events::ControlEvent` today emits only
  `hello | output | exit | activity | message | liveness | command | supervisor | state`; none
  cover the queue. The project-registry side already has the analogue — a write flips the GUI
  host's dirty flag via `mark_projects_dirty()` — so #9 follows that pattern for queue mutations.

## How queues map to projects today

There is **no hard link** in the schema. A `Task` owns an opaque `queue` namespace string and
an opaque `payload`; a `Project` owns `{ id, path, name, color, lastOpenedAt }`. The queue core
never parses either, by design ("opaque at storage, typed at the edges").

So the panel maps queue → project **by naming convention**: a project derives its queue name
from its `id` (or path), and the panel filters `GET /queues` / `GET /queues/{queue}/tasks` to
that name. Formalizing this mapping (a stored `queue` field on `Project`, or a documented
derivation rule) is the prerequisite that lets #6–#9 attribute work to the right project row.
