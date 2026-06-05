# Agent orchestration on hyperpanes — gap analysis & design

How to run an LLM **agent org** on top of hyperpanes: a manager driving worker panes, or a
recursive **CEO → manager → workers** tree. This builds on the control API (M2/M2b — see
[`cli-multiwindow-mcp-plan.md`](cli-multiwindow-mcp-plan.md)) and the separate MCP project at
`C:\hyperpanes-mcp`.

**Status (2026-06-05): Phases A, B & C ALL BUILT & static-verified** across both repos
(hyperpanes typecheck + 141 unit tests + `electron-vite build`; MCP typecheck + 64 unit tests +
`tsc` build, all green).
- **Phase A** — pane-id env (A), activity (B), meta (C), `open_pane`→paneId (D).
- **Phase B** — message bus (E: durable inbox, `/panes/:id/messages`, scope-filtered `message`
  events, `send_message`/`read_messages`/`send_to_parent`/`broadcast_subtree` + a subscribable
  messages resource), capability scoping (F: `POST /tokens`, scope enforced on every route, a
  scope-filtered `/state` + event stream, scoped-token-via-env that suppresses the master
  control file), and `whoami`.
- **Phase C** — clean output (G: `?strip=1`), advisory write lock (H: `POST/DELETE
  /panes/:id/lock` gating `send_input`).

The live socket round-trip (app running + control enabled + MCP connected) is still
**unverified** — everything is static + unit-tested only. The meaningful end-to-end smoke:
spawn a pane, watch its `activity` flip to `idle`, read its `meta`; then mint a scoped token,
launch a scoped child via `open_pane` env, and exchange messages parent↔child.

**Design decisions taken during the B/C build** (the plan's open questions, resolved):
- *Message delivery* → **durable per-pane inbox, at-least-once, monotonic-seq cursor reads +
  live push nudge** (a late/reconnecting node still reads its backlog). Bounded per pane.
- *Scoped-token security* → a child is handed `HYPERPANES_CONTROL_TOKEN`/`_PORT` via pane env,
  and that **suppresses the `HYPERPANES_CONTROL_FILE` injection**, so a scoped worker can never
  read the master token from `control.json`. Scoped tokens are in-memory, optionally TTL'd, and
  may only mint *narrower* sub-tokens (no escalation), validated against the live tree.
- *Dumb workers* keep the `send_input`/`read_pane` fallback; the message bus + scoping are for
  MCP-capable nodes. Inbox/lock state for a closed pane lingers until app restart (bounded).

## Hard requirement: topology flexibility

The design must serve **both** shapes without favoring either, and must allow mixing them:

- **(A) Single external orchestrator** — one LLM (e.g. Claude Code holding the MCP) drives N
  worker panes. No worker manages anyone.
- **(B) Recursive agent-managers** — a manager is *itself* an agent running in a pane, which
  spawns and drives its own sub-workers; nest arbitrarily (CEO → managers → workers).

Consequences that shape every primitive below:

1. **Hierarchy is data, not API.** The org chart lives in per-pane metadata + the natural
   window→tab→pane tree, never hard-coded into endpoints. A flat manager→workers and a deep
   CEO→…→worker use the *same* primitives.
2. **Every node is addressable and self-aware.** A pane-agent must know *who it is* and *how
   to reach the control plane*, or (B) is impossible.
3. **Access is capability-scoped but defaults to full.** The root holds the master token (no
   scope); a parent may mint a *narrower* token for a child. (A) just uses the master token;
   (B) hands each manager a subtree-scoped token. Scoping is opt-in, never required.
4. **Two node kinds, two comms paths.** An **MCP-capable** node (a real agent CLI with the
   hyperpanes MCP configured) talks structured messages over the control plane. A **dumb**
   process (no MCP) is driven by `send_input` and observed by reading its output. The design
   supports both; structured beats scraping wherever available.

## The model

- **Pane = worker.** Each is a terminal running an agent CLI (or a plain command).
- **Org tree = window → tab → pane.** `GET /state` already returns `windowId / tabId /
  tabTitle / activeTab`, so grouping (division/team/worker) is readable for free.
- **Control plane = the loopback control API.** Multiple clients may connect (token-gated
  HTTP + `/events` WS), which is what makes (B) possible at all.

## Current coverage (what exists today)

| Capability | Mechanism | Status |
| --- | --- | --- |
| Enumerate workers | `list_panes` / `GET /state` | ✅ |
| Read a worker's output | `read_pane(tail)` + subscribable output resource (WS) | ✅ (raw ANSI) |
| Instruct a worker | `send_input` (triple-gated) | ✅ |
| Spawn / kill / restart | `open_pane` / `close_pane` / `restart_pane` | ✅ |
| Re-arrange / focus | `set_layout`, `focus_pane` | ✅ |
| Pre-build a topology | workspace JSON (now fully granular) | ✅ |
| Stable addressing | `paneId` survives restart | ✅ |
| Process death signal | `status`/`exitCode` + `exit` event | ✅ |
| **Worker "done/waiting" signal** | `activity: busy/idle/exited` field + `activity` event (heuristic) | ✅ (Phase A) |
| **`open_pane` → new paneId** | `/command` request/response (correlationId) | ✅ (Phase A) |
| **Structured role/identity** | `meta` map (`role`/`parent`/`agentType`/`task`) + `set_meta` | ✅ (Phase A) |
| **Pane self-awareness (who am I)** | `HYPERPANES_PANE_ID` + `HYPERPANES_CONTROL_FILE` env | ✅ env (Phase A); `whoami` tool is Phase B |
| **Inter-node messaging** | durable per-pane inbox + `send_message`/`read_messages` + bus events | ✅ (Phase B) |
| **Scoping / ownership** | `POST /tokens` scoped tokens, enforced on every route + scoped events | ✅ (Phase B) |
| **Clean (de-ANSI'd) output** | `GET /panes/:id/output?strip=1` + `read_pane(strip)` | ✅ (Phase C) |
| **Advisory write lock** | `POST/DELETE /panes/:id/lock` gating `send_input` | ✅ (Phase C) |

## The additions

Grouped; each notes which side it touches — **app** = `C:\hyperpanes` control plane,
**MCP** = `C:\hyperpanes-mcp` tool surface.

### A. Pane self-awareness — the recursion enabler *(app)*
On spawn, inject env into the pane's pty (`src/main/session.ts` builds `env`; `session:spawn`
in `ipc.ts`):
- `HYPERPANES_PANE_ID` — the pane's own id.
- `HYPERPANES_CONTROL_FILE` — path to the discovery file (or, when scoping is on, a
  pre-minted **scoped token** so the child can only touch its subtree).
An MCP-capable agent launched in that pane then auto-knows its identity and how to reach the
control plane — without which a manager-in-a-pane can't act. A `whoami` MCP tool returns
`{ paneId, role, parent, scope }` from env + `/state`.

### B. Liveness / activity *(app)*
Surface the **already-built** idle detector (`src/renderer/store/useIdle.ts`, the idle-glow
quiescence heuristic) into the control plane:
- Add `activity: 'busy' | 'idle' | 'exited'` to `ControlPaneInfo` (publish from
  `buildControlPayload` in `control.ts`).
- Emit an `{ type:'activity', paneId, activity }` frame on `/events` when it flips.
- **Heuristic, document it as such:** "idle" = no output for N seconds (the agent is likely
  waiting at its prompt / done). Not a contract that work is complete. This is the single
  highest-leverage add for orchestration — it's how a manager knows a worker is ready for the
  next instruction without scraping.

### C. Structured pane metadata *(app + MCP)*
Add `meta?: Record<string, string>` to `PaneSpec` (workspace JSON + `open_pane`) and to the
live pane + `ControlPaneInfo`. Reserved keys give the org its shape:
- `role` (e.g. `ceo` / `manager:frontend` / `worker`), `parent` (parent paneId),
  `agentType` (`claude` / `aider` / …), `task` (current assignment) — rest free-form.
A `set_meta(paneId, meta)` command + `meta` on `open_pane` make the org **self-describing for
any reader**, which is what lets the same API serve (A) and (B).

### D. `open_pane` returns the new paneId *(app + MCP)*
Today `/command` is fire-and-forget (`{ ok }`); `applyControlCommand`'s `newPane` calls
`addPane` which *does* return the id, but it's dropped. Add a command result round-trip:
attach a `correlationId` to dispatched commands, have the renderer reply
`control:commandResult { correlationId, result }`, and resolve the `/command` HTTP response
with it. Lets a manager spawn workers concurrently and map each to its id (no racy
list-diff).

### E. Message bus — structured inter-node comms *(app + MCP)*
The neutral transport that replaces scraping for MCP-capable nodes. Per-pane inbox:
- `POST /panes/:id/messages { from, body }` — enqueue.
- Delivery: for an MCP-capable target, push `{ type:'message', to, from, body }` on its
  `/events` stream and expose `read_messages` / `send_message` MCP tools. For a **dumb**
  worker, the orchestrator falls back to `send_input` (inject) + `read_pane` (observe).
- Hierarchy helpers built on `meta`: `send_to_parent`, `broadcast_subtree` resolve targets
  from `parent`/tree — but the bus itself is hierarchy-agnostic (any pane → any pane), so it
  serves both topologies. **Open question:** delivery semantics (at-least-once? ack? cursor
  vs push) — see Risks.

### F. Capability scoping *(app + MCP)*
Opt-in, so (A) ignores it and (B) relies on it:
- `POST /tokens { scope: { windowIds?|tabIds?|paneIds? }, ttl? }` → a scoped token. The
  server tags every token with its scope and enforces it on all routes (read, command,
  input, messages). Master token (in `control.json`) = unscoped = the root/CEO.
- A parent mints a child token covering the child's subtree + its own inbox and passes it via
  env (A). A worker thus *cannot* close the CEO's pane.

### G. Clean output mode *(app + MCP)*
`GET /panes/:id/output?tail=N&strip=1` → ANSI-stripped text, for a manager parsing a worker's
TUI. Cheap. (Structured messaging is the real answer; this is for observability / dumb
workers.)

### H. Concurrency *(app)*
Advisory `POST /panes/:id/lock` to serialize `send_input` when multiple managers might write
the same pane. Low priority; until then document "one writer per pane."

## How it composes (proof of flexibility)

- **(A) single orchestrator:** master token; `list_panes` + `meta.role` to see the org;
  `open_pane(meta)` to staff it; `activity` event to know when a worker is ready;
  `send_input`/`read_pane` (or messages, if workers are MCP-capable). Scoping/whoami unused.
- **(B) recursive org:** launch the tree from workspace JSON (windows=divisions,
  tabs=teams, panes=agents, `meta` = roles/parents). Each manager-pane boots MCP-capable,
  reads `HYPERPANES_PANE_ID` + its scoped token (`whoami`), spawns sub-workers within scope,
  and coordinates over the message bus (`send_to_parent` / `broadcast_subtree`). Same
  primitives, more levels.
- **Mixed:** a manager may drive some MCP-capable sub-managers (messages) and some dumb
  command workers (`send_input`) side by side. Nothing in the API forbids it.

## Phased plan

- **Phase A — cheap, high-impact (unblocks solid single-orchestrator + makes the org
  self-describing):** B (activity), C (meta), D (open_pane→id), and the env half of A
  (`HYPERPANES_PANE_ID`). All small; the activity detector already exists.
  **BUILT & static-verified 2026-06-05** (both repos green; live socket smoke still pending —
  see the status note at the top).
- **Phase B — recursion & coordination:** E (message bus), F (scoping + scoped-token env), the
  `whoami` tool. **BUILT & static-verified 2026-06-05** — messaging-semantics decision recorded
  in the status note above (durable, at-least-once, cursor-based).
- **Phase C — polish:** G (clean output), H (locking), hierarchy convenience tools
  (`send_to_parent`/`broadcast_subtree`). **BUILT & static-verified 2026-06-05.**

## Implementation — Phase A (file-level)

The cheap bundle: **pane-id env (A)**, **activity (B)**, **meta (C)**, **open_pane→id (D)**.
Keep the MCP (`C:\hyperpanes-mcp`) in lockstep — its zod is `.strict()`, so any new
`PaneSpec`/`ControlPaneInfo` field must be added there too or it rejects valid input. Gate
each change with `npm run typecheck && npm test && npm run build` on both repos.

### A1 — `HYPERPANES_PANE_ID` env *(small)*
- `src/renderer/components/Terminal.tsx` (~L362): add `paneId` to the `window.hp.spawn({…})`
  call (the `paneId` prop is in scope).
- `src/preload/index.ts` `SpawnOptions` + `src/renderer/global.d.ts` `HpSpawnOptions`: add
  `paneId?: string`.
- `src/main/session.ts`: `SpawnOptions` add `paneId?`; in the env block add
  `if (opts.paneId) env.HYPERPANES_PANE_ID = opts.paneId;` and
  `env.HYPERPANES_CONTROL_FILE = join(app.getPath('userData'),'control.json')` (path always;
  may not exist until control is enabled — the agent checks). Scoped token is Phase B.

### B — activity (busy/idle/exited) *(medium)*
- **Decouple tracking from the glow:** in `src/renderer/store/useIdle.ts`, drop the
  `if (!s.idleAlert) return` early-out in `markActivity` so idle is tracked regardless of the
  visual setting; verify `PaneFrame.tsx` (L83 area) still gates the *glow* on `idleAlert` so
  the visual is unchanged.
- `ControlPaneInfo` (`src/main/control-server.ts` **and** `src/renderer/types.ts`): add
  `activity: 'busy' | 'idle' | 'exited'`.
- `buildControlPayload` (`src/renderer/control.ts`): per pane,
  `status==='exited' ? 'exited' : useIdle.getState().idle[p.id] ? 'idle' : 'busy'`.
- **Re-publish on idle flips:** the control-publish effect in `App.tsx` only subscribes to
  `useWorkspace`; add a `useIdle.subscribe(...)` that triggers the same debounced publish, or
  idle changes won't propagate.
- **Event:** add `{ type:'activity'; paneId; activity }` to `ControlEvent`; in
  `setWindowState`, diff incoming pane activity vs the prior snapshot and `broadcast` an
  `activity` event per flip (the coalesced `state` ping already fires too).
- MCP: `model.ts` `ControlPane` + `ControlEvent` add `activity`; `list_panes` /
  `control_status` surface it; `subscriptions.ts` may forward `activity` as a notification.

### C — `meta` map *(medium)*
- `PaneSpec` (renderer `types.ts`, main `workspace.ts`, `preload/index.ts`) + `Pane`
  (`types.ts`): add `meta?: Record<string,string>` (reserved keys: `role`, `parent`,
  `agentType`, `task`).
- `groupFromSpec` carry `meta`; `specFromGroup` emit `...(p.meta ? { meta: p.meta } : {})`.
- New store action `setPaneMeta(paneId, meta)` (shallow-merge) on `WorkspaceState`; `addPane`
  already takes `Partial<Pane>`, so spawn-time meta flows once `Pane` has the field.
- `ControlPaneInfo` + `buildControlPayload`: include `meta`.
- `applyControlCommand` (`control.ts`): `case 'setMeta'` → `ws.setPaneMeta(paneId, cmd.meta)`.
- MCP: `schema.ts` `PaneSpecSchema` add `meta: z.record(z.string()).optional()`;
  `control-tools.ts` add `meta` to `open_pane` + a `set_meta` tool; `model.ts` `ControlPane`
  add `meta`; `list_panes` surface it.

### D — open_pane returns the new paneId *(medium; the only non-trivial wiring)*
Turn `/command` into request/response:
- `ControlCommand` gains an optional `correlationId`. `ControlDeps.dispatchCommand` becomes
  `(windowId, cmd) => Promise<{ ok: boolean; result?: unknown }>`.
- `ipc.ts` `dispatchCommand`: mint a `correlationId`, keep a `Map<id, resolver>` with a ~2s
  timeout, `webContents.send('control:command', cmd)`, return the promise. Add an
  `ipcMain.on('control:commandResult', …)` that resolves it.
- `applyControlCommand` returns a value for result-bearing commands (`newPane` → the id from
  `ws.addPane(...)`, which already returns it); `App.tsx` `onCommand` replies via a new
  preload `control.commandResult(correlationId, result)` when `cmd.correlationId` is set.
- `control-server` `/command` handler `await`s `dispatchCommand` and returns
  `{ ok, result }`.
- MCP: `client.command` returns `{ ok, result }`; `open_pane` returns `{ ok, paneId: result }`
  (drop the "call list_panes" hint).

## Open questions / risks

- **Liveness is heuristic.** Output-quiescence ≈ "waiting", not a guaranteed "task complete".
  Agents that stream/think silently could read as idle; chatty ones never idle. May need a
  per-agentType tuning or an explicit "I'm done" convention workers can emit.
- **Terminal scraping is lossy** (ANSI/TUI redraws). Strip mode helps; structured messages
  are the durable fix but require MCP-capable workers.
- **Message delivery to dumb workers** degrades to `send_input` — no real inbox. Accept, or
  require orchestrated workers to be MCP-capable.
- **Scoped-token security:** tokens travel via pane env; a compromised worker leaks its
  (already limited) scope. Keep loopback-only + short TTL; never put the master token in a
  worker's env.
- **Who is root?** In (A) the human/MCP host. In (B) a designated CEO pane holds the master
  token — decide how it's granted (it shouldn't be a normal worker).
- **Backpressure:** a flooding worker vs the manager's context window — `tail`, sampling, or
  summarizer panes.
