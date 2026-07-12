# Goals system for projects — gap analysis & design

A per-project **goal** layer, run as a **standing agent org** (not a Rust driver loop). A goal is
free text you type; a long-lived per-project **goals orchestrator** owns the goal list; it spawns a
per-goal **planner** (opus[1m]) that fans out **worker** panes (sonnet[1m]); workers report up,
planner reports to the goals orchestrator, the orchestrator adds/updates goals, and the loop
continues 24/7. Durable subtask execution rides the existing work queue; the org survives crashes
via the session daemon + `claude --resume`; it keeps running past account limits by rotating across
three Claude accounts.

**Status (2026-07-12): DESIGN — not built.** Nearly every mechanism this needs already exists
(CEO→manager→worker org, message bus, capability scoping, work queue, worker runner, session
daemon, supervisor, Claude resume, `fan-out` / `use-hyperpanes` skills). What's missing is: the
palette entry point + find-or-spawn glue, the goal-org agent roles/skills, **account rotation**, and
the **dormant queue plumbing** that makes subtask execution durable. This doc is the design after a
grilling pass; open decisions are all resolved (see "Decisions").

---

## Topology

```
Ctrl+Shift+P → "New goal…" → pick project → free-text intent
   │
   ▼  find-or-spawn by meta {role=goals-orch, project=<path>}
Goals orchestrator        headless · per-project · launched in project cwd · LONG-LIVED · loops
   │   owns the goal list; ingests new goals + planner reports; add/updates goals
   ▼  spawns one per goal
Planner orchestrator      opus[1m] · per-goal · headed · torn down when goal terminal
   │   decomposes intent → fan-out
   ▼
Worker panes              sonnet[1m] · per-subtask ──report──▶ planner ──report──▶ goals-orch → loop
```

Every arrow is an **existing** primitive: find-or-spawn = `list_panes` + `meta` filter + `open_pane`
(`readmodel.rs` `meta.role` convention); up/down reporting = message bus (`send_to_parent`,
`broadcast_subtree`, `read_messages`); hierarchy = `meta` keys `role`/`parent`/`project`/`goal`
(`agent-orchestration-plan.md:100` "hierarchy is data, not API"); fan-out = the `fan-out` skill;
sandboxing = scoped tokens (`scope.rs`, `tokens.rs`).

---

## Decisions (from grilling)

| # | Question | Decision |
|---|---|---|
| Topology | driver = Rust loop vs agent org | **Agent org** (re-planning loop; planner reports → orchestrator re-plans) |
| Goals orch | scope | **Per-project**, launched in **project cwd**, long-lived, **multiple concurrent goals** |
| Planner | lifetime | **Per-goal**: spawn → run → tear down |
| Goal state | store vs agent-held | **Agent-held** in the orchestrator's resumed conversation; durable *execution* in the work queue; completion notes in `ProjectMemory.timeline`. A queryable `goals.json` is **deferred** (only needed for a future GUI list) |
| Wedge detection | Rust heuristic vs LLM | **LLM decides** — the orchestrator/watchdog reads the pane (`read_pane` + liveness) and judges stuck vs working. No brittle output-quiescence timer |
| Budget | cap on tokens/$ | **No budget breaker** (explicit). Stops are: acceptance-pass, human cancel, or all accounts exhausted |
| 24/7 survival | how to outlast limits | **Rotate 3 Claude accounts** on session/weekly limit, per-pane |
| Models | which tier | planner opus[1m], workers sonnet[1m]; goals-orch = default opus (no budget concern) |

> ⚠️ **Surfaced, accepted:** no budget breaker + re-planning loop + auto-rotate means a wedged or
> looping goal can silently drain **all three accounts** unattended. Intended — the watchdog +
> account-health notifications are the only guardrails.

---

## What exists to build on

| Capability | Where | Role in goals |
|---|---|---|
| CEO→manager→worker org: message bus, `meta` hierarchy, scoped tokens, `whoami` | `control/inbox.rs`, `scope.rs`, `tokens.rs`; `agent-orchestration-plan.md` | The org itself — goals-orch/planner/worker map straight onto it |
| `fan-out` + `use-hyperpanes` skills | `~/.claude/skills/*` (symlinks) | Planner uses fan-out to spawn workers; orchestrators use use-hyperpanes to drive panes |
| Find-or-spawn by meta | `readmodel.rs` (`meta.role`), `open_pane`/`list_panes` MCP | Palette locates the project's goals-orch or creates it |
| Work queue (SQLite, states, fencing, leases, backoff, dedupe) | `control/work.rs` | Durable subtask execution — planner enqueues, workers drain |
| Worker runner (`hyperpanes worker`, `--count`, `--worktree`, `HP_TASK_*`) | `app/src/worker.rs` | Runs each subtask, git-worktree-isolated |
| Session daemon (PTYs survive GUI crash, re-attach by uid) | `session/daemon.rs` | Keeps the whole org alive across a GUI crash |
| Claude resume (`--resume`, session marker, prompt queue) | `resume_queue.rs`, `claude_panes.rs`, `dispatch.rs:213` | Orchestrator survives app relaunch with goal list intact; watchdog restarts a wedged agent **without losing its conversation** |
| Supervisor (auto-restart on exit, backoff, `maxRetries`) | `supervisor.rs`, `server.rs:547` | Restart-on-crash for worker panes |
| Liveness (`working\|awaiting-input\|done\|exited`) | `server.rs:219`, `session/osc133.rs` | The signal the LLM watchdog reads |
| Project identity + memory | `persistence/projects.rs`; `ai/ai_store.rs` (`ProjectMemory.timeline`, `Milestone`) | Project picker source; goal milestones land in the timeline |
| Spawn env hook | `session_manager.rs:157` (`SpawnOptions.env: Option<EnvMap>`) | Injects per-pane `CLAUDE_CONFIG_DIR` for account rotation |
| Palette command enum | `app/src/palette.rs:109`, `app/src/command.rs:22` (`Command::NewPane`) | Where "New goal…" hooks in |

---

## Gaps to close

### A. Account rotation — the load-bearing 24/7 mechanism

**Current disk reality (verified):** `~/.claude` (acct 1, 137 transcript dirs) and `~/.claude-alt`
(acct 2, **own separate** `projects/`+`sessions/`, 1 dir). Only 2 dirs exist; 3rd is TODO. `claude`
stores transcripts **under `CLAUDE_CONFIG_DIR`**, and hyperpanes sets **no** `CLAUDE_CONFIG_DIR`
today (grep-confirmed). So rotating accounts today **silently starts a fresh conversation** — the
per-pane + resume-across-accounts requirement is currently unsatisfiable.

**Required layout** (per-account credentials, one shared transcript store so `--resume` works across
accounts):

```
~/.claude-shared/projects   ~/.claude-shared/sessions        # single real transcript store
~/.claude       → creds A ;  projects,sessions → symlink → shared     (acct 1)
~/.claude-alt   → creds B ;  projects,sessions → symlink → shared     (acct 2)
~/.claude-alt2  → creds C ;  projects,sessions → symlink → shared     (acct 3, CREATE)
```

Wiring:
- **Migration** (one-time script): create `~/.claude-shared`, move `~/.claude/projects`+`sessions`
  in, symlink both back; do the same for alt (merge its 1 dir); create the 3rd account dir. Share
  everything **except** `.credentials.json` (and per-account `.claude.json`/`history.jsonl` if
  desired — user confirmed share-all-but-creds is fine).
- **Account-health map** (global, in the control plane): `{dir → {healthy, exhausted_until}}`. All
  panes consult it; a pane never spawns on a known-dead account.
- **Detection:** watch pane output for the CLI's rate/weekly-limit message → mark that account
  exhausted (with reset time if parseable) in the health map. (Reuse the OSC/output tap the liveness
  layer already reads.)
- **Rotation (per-pane):** on spawn or on detected exhaustion, pick the next healthy account and set
  `SpawnOptions.env[CLAUDE_CONFIG_DIR] = <dir>`. Because transcripts are shared, a restarted pane
  can `--resume` its session under the new account.

### B. Dormant queue plumbing — durable subtask execution (do first, cheap)

All fns exist in `work.rs`; nothing calls them.

1. **Persist on disk:** add `paths::work_db() -> data_dir().join("work.db")` (sibling of
   `projects_json`); switch `Shared::default` from `open_in_memory()` to
   `WorkQueue::open(work_db())`. Today the queue is **lost on app restart**.
2. **Boot recovery:** call `recover_in_flight` (`work.rs:619`) on startup.
3. **Reaper tick:** one `tokio::interval` in `server.rs` (mirror `server.rs:421`) calling
   `reap_expired` (`work.rs:599`) so a dead worker's lease reclaims its task.
4. **Worker-exit requeue:** hook `requeue_worker` (`work.rs:610`) from the session Exit arm.

### C. Goal org roles + entry point

5. **Palette "New goal…"** — new `Command::NewGoal` (`command.rs`) + palette row (`palette.rs:109`):
   opens a small form (project picker from `list_projects`, free-text intent). On submit: find the
   project's goals-orch via `list_panes` filtered `meta.role=goals-orch && meta.project=<path>`;
   if present, inject the intent as a message / prompt; else `open_pane` in the project cwd running
   the goals-orch skill, `set_meta` its role/project, then inject.
6. **Three agent role definitions** (skills or `--append-system-prompt` personas):
   - *goals-orchestrator* — headless, loops: hold goal list in conversation; for each new/updated
     goal spawn a planner; ingest planner reports; add/update goals; run the **wedge watchdog**
     (periodically `read_pane` its planners/workers, judge stuck, re-prompt or `restart_pane
     resume:true`, escalate after N).
   - *planner* — opus[1m], per goal: decompose intent → enqueue subtasks (with `depends_on` DAG) →
     `spawn_workers` / fan-out → collect worker reports → **re-plan** on surprise → report status to
     goals-orch → exit when goal terminal.
   - *worker* — sonnet[1m], per subtask: claim via the runner, do the work in its worktree, report
     result up, ack/nack.
7. **Acceptance:** the planner enqueues a final verification subtask (`depends_on` = all build
   subtasks) whose exit code / LLM-judge verdict decides goal `Done` vs re-plan. Success is **criteria
   met**, not "exit 0". On `Done`, orchestrator appends a `Milestone` to `ProjectMemory.timeline`.

### D. DAG (optional for v1)

8. Add `goal_id: Option<String>` + `depends_on: Option<String>` (JSON array) to `work.rs` `Task`
   (schema v1→2 via the existing `schema_version` guard); extend the `claim` predicate so a task is
   unclaimable until every dep is `Done`. Lets the planner express ordered subtasks natively. If
   deferred, the planner enforces ordering itself by enqueuing the next wave only after the prior
   wave's reports arrive.

---

## Build order

1. **Queue plumbing (B)** — `work_db()` path, disk-backed `Shared::default`, boot recovery, reaper
   tick, worker-exit requeue. Pure reuse; unblocks durable subtask execution. *(Rust)*
2. **Account rotation (A)** — migration script + shared-transcript layout + 3rd account dir; global
   account-health map; output-watch detection; per-pane `CLAUDE_CONFIG_DIR` injection at spawn.
   *(script + Rust)*
3. **Goal org roles + palette (C)** — `Command::NewGoal` + form + find-or-spawn glue; the three
   agent personas/skills (goals-orch, planner, worker) including the wedge watchdog and acceptance
   subtask. *(Rust glue + skills, mostly no new engine)*
4. **DAG (D)** — `goal_id`/`depends_on` columns + claim gate. *(Rust, optional)*
5. **Later:** queryable `goals.json` + a GUI goals panel; cron/recurring goal triggers on
   `available_at`.

---

## Verification

- **Unit (Rust):** `work_db` durability across reopen (extend `work.rs:1193` test off in-memory);
  reaper requeues an expired lease; worker-exit requeue; (if D) `depends_on` blocks claim until deps
  `Done`. Account-health map: exhausted account skipped, resets after `exhausted_until`.
- **Integration (headless, `crates/core/src/bin/headless.rs`):** enqueue subtasks with a DAG →
  `hyperpanes worker --count N` drains in dep order → acceptance task flips terminal; kill a worker
  mid-task, assert reaper requeues; simulate a limit-message on a pane, assert the health map marks
  the account exhausted and the next spawn picks another dir.
- **Live (GUI + MCP):** Ctrl+Shift+P → New goal on a real project → confirm goals-orch spawns in the
  project cwd (role/project meta set), planner spawns per goal, workers fan out, goal reaches `Done`,
  and a `Milestone` lands in the project timeline. Crash the GUI mid-goal → daemon keeps the org's
  PTYs alive, relaunch `--resume`s the orchestrator with its goal list. Force acct A's limit message
  → a worker restarts under acct B and `--resume`s its session (proves shared-transcript rotation).
