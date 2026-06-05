# Plan — expanded CLI, launch-time multi-window, and an MCP control surface

Roadmap for three connected workstreams. Two live in **this repo** (hyperpanes); the
**MCP server is a separate project** that sits on top of the control surface we add here.

**Locked design decisions (2026-06-05):**

1. **MCP scope — both, phased.** Phase 1 is stateless launch-config generation (compose
   a workspace + shell out to `hyperpanes`). Phase 2 adds live control of a running
   instance (read/inspect panes, send input, mutate layout).
2. **Instance model — single canonical process.** Add `app.requestSingleInstanceLock()`;
   a second `hyperpanes …` invocation (and every MCP call) routes into the *running*
   process and adds windows/tabs instead of starting a rival process.
3. **CLI scope — per-pane flags + `--tab` / `--window` separators.** Full multi-tab /
   multi-window parity with the JSON format; JSON stays the canonical format for large
   setups, the CLI grammar compiles down to the same structure.

## Why these connect

The single-instance lock is the keystone. Launch-time multi-window and MCP both need a
canonical running process to target ("open this in my running session", "the agent drives
*the* app"). The expanded CLI and the JSON `windows` layer must produce the **same**
in-memory structure so there's one seeding code path. MCP Phase 2 is a thin adapter over a
local control API we expose from `main`.

Sequence: **schema + single-instance → CLI → control API → MCP (P1 then P2)**.

## Build status (2026-06-05)

**M0, M1, M2 BUILT & static-verified** (typecheck + 108 unit tests + `electron-vite build`
all green); **manual GUI pass still pending**. M3/M4 (the separate MCP project) not started —
handoff doc in OS temp (`hyperpanes-mcp-handoff.md`).

- **M0** — `WindowSpec` layer + `windowsOf` normalizer (`src/main/workspace.ts`,
  `src/renderer/types.ts`); `requestSingleInstanceLock` + `second-instance` routing
  (`src/main/index.ts`); per-window push-seed via `getSeed.windowSpec`
  (`ipc.ts`/`window.ts`/`App.tsx`). Bounds + cascade placement honored.
- **M1** — `parseCli` rewritten as a window→tab→pane state machine with `--window`/`--tab`
  separators + per-pane `--cwd`/`--shell`/`--font`; back-compat legacy shape preserved
  (`workspace.test.ts`, 16 cases).
- **M2** — `src/main/control-server.ts`: loopback HTTP, token + `userData/control.json`
  discovery, **off by default**, `allowInput` second gate. Renderers publish structure
  (`control.ts` + `App.tsx`); Preferences → General has the opt-in toggles.
- **M2b — WebSocket event stream BUILT (2026-06-05).** `/events` WebSocket on the same
  port + token (via `?token=`), using `ws`. Pushes `{type:'output',sessionUid,paneId,data}`,
  `{type:'exit',…,code}`, and a coalesced `{type:'state'}` re-fetch ping; greets with
  `{type:'hello',pid,version}`. `control.json` now includes the full `events` ws URL. Output/
  exit are teed from ipc's session handlers (no-op when no client connected); a `sessionUid→
  paneId` reverse index (rebuilt only on structure change) keeps the per-batch path O(1).
  Poll route `GET /panes/:id/output?tail=N` remains for the initial backlog.

**Total launch granularity — DONE (2026-06-05).** `GroupSpec` gained `sizes` (per-pane
split fractions), `mainFraction` (Main+Stack split), and `focused`/`zoomed` (pane indices),
so a launched/restored tab reproduces its exact split + which pane is focused/maximized.
`groupFromSpec` consumes them (defensively validated → defaults on bad input; auto layout
stays equal); `specFromGroup` emits them only when non-default. Mirrored in the MCP schema
(`C:\hyperpanes-mcp\src\schema.ts`) + flagged CLI-lossy in `compile-cli.ts`. JSON/file launch
only — no CLI flags. Still not settable: nothing left on the wish-list; per-pane zoom + split
ratios + intra-tab focus are now all expressible.

**Multi-window restore — DONE (2026-06-05).** Every window now publishes its tabs to main
(`workspace:windowSession`); main aggregates by window id, captures per-window bounds, and
writes a `windows[]` `last-workspace.json`, so a relaunch restores all windows (with
positions). Empty-guard prevents wiping; a `quitting` flag stops cascading closes from
shrinking the session; closing one window (app up) drops just that window. Replaced the old
primary-only `serializeSession`/`saveLast` path. (109 unit tests.)

## Current state (grounding)

- `parseCli` (`src/main/workspace.ts`) is single-tab only: per-pane `-c`/`-l`/`--color`;
  `--cwd`/`--shell` are launch-wide. It cannot emit the `groups` array the file format
  already supports, and there is no window concept anywhere.
- `WorkspaceFile` = `{ name?, layout?, panes[], groups?[], active? }`;
  `GroupSpec` = `{ title?, layout?, panes[] }`;
  `PaneSpec` = `{ label?, subtitle?, color?, command?, cwd?, shell?, fontSize? }`.
- Runtime multi-window already works: `spawnWindow()` (`src/main/ipc.ts`) makes
  per-window session-owned windows for tear-off, all in **one** process.
- **No** `requestSingleInstanceLock` (`src/main/index.ts`) → a second launch starts a
  separate OS process.
- Launch seed is pull-based and process-global: the renderer calls
  `workspace:getInitial` → `getInitialWorkspace()`. There is no per-window seeding.
- **No** external control channel; IPC is internal main↔renderer only.

## M0 — schema + single-instance foundation (this repo)

Add a **window** layer above tabs (groups). Back-compatible: absent `windows` ⇒ the
top-level `panes`/`groups` describe a single window.

```jsonc
WorkspaceFile {
  name?,
  // legacy single-window fields kept for back-compat:
  layout?, panes[], groups?[], active?,
  // new:
  windows?: WindowSpec[]
}
WindowSpec {
  title?,
  active?: number,                 // active tab index
  bounds?: { x?, y?, width?, height?, maximized?, fullscreen? },
  groups: GroupSpec[]              // tabs in this window
}
```

Work:
- Extend types in `src/main/workspace.ts` (`WorkspaceFile`, add `WindowSpec`) and mirror in
  `src/renderer/types.ts`. `resolveCwds` must also walk `windows[].groups[].panes`.
- `src/main/index.ts`: `requestSingleInstanceLock()`; quit if not primary; on
  `second-instance(_e, argv, cwd)` parse that argv and spawn the declared windows into the
  running process. Watch argv-slicing differences (dev vs packaged) and macOS `open-file`.
- **Per-window seeding (push, not pull):** extend `spawnWindow` to accept a window-level
  seed (multiple tabs + active index), not just the single-`GroupPayload` tear-off seed.
  Main computes the window list and calls `spawnWindow` once per `WindowSpec`;
  `workspace:getInitial` stays as the fallback for the very first window only.
- Default routing: a launch opens its **declared windows** (new windows). A
  `--into-current` (later) can merge as tabs into the active window.
- Persistence: `serializeSession` (`src/renderer/workspace/serialize.ts`) currently
  snapshots one window. Decide: extend autosave/restore to multi-window, or keep restore
  single-window for M0 and only honor `windows` on explicit file/CLI launch.
- Tests: window round-trip + per-window seeding (extend `workspace.test.ts`).

## M1 — expanded CLI (this repo)

Rewrite `parseCli` into a small **window → tab → pane** state machine. Separator tokens
segment the pane stream:

```bash
hyperpanes \
  --window --name app --layout main-stack \
    -c "npm run dev" -l server --color "#e5484d" --cwd ./app --shell pwsh \
    -c "tail -f log"  -l logs   --font 12 \
  --tab --name tests --layout columns \
    -c "vitest" -l unit \
  --window --name db \
    -c "psql mydb" -l psql --cwd ./db
```

Rules:
- `--window` starts a new window (own `--name`/title, `--layout`, optional `--active`).
- `--tab` starts a new tab (group) in the current window.
- `-c`/`--command` adds a pane to the current tab; `-l/--label`, `--color`, `--cwd`,
  `--shell`, `--font` attach to the most recent `-c`.
- **Back-compat:** with no separators it's one tab / one window (today's behavior).
  `--cwd`/`--shell` set before the first `-c` remain launch-wide defaults, overridable
  per-pane after a `-c`.
- Output: `parseCli` now returns `windows[]`; both CLI and JSON funnel into the same
  M0 seeding path.
- Tests: extend `workspace.test.ts` for separators, attachment, and back-compat.

## M2 — local control API (this repo; prerequisite for MCP Phase 2)

A loopback control server in `main`, **off by default** behind a setting.

- Transport: `127.0.0.1` HTTP + WebSocket (reachable from a Node MCP server in any
  language). Per-instance bearer token.
- Discovery: write `{ port, token, pid }` to `userData/control.json` on start (single
  instance ⇒ exactly one file). The CLI and MCP server read it to find the running app.
- Read surface: `listWindows/Tabs/Panes`, `readPane(tail N | scrollback)`,
  `paneStatus(running/exitCode)`, `getSelection`.
- Control surface: `spawn{Window,Tab,Pane}(spec)`, `close{Pane,Tab,Window}`, `focusPane`,
  `setLayout`, `rename/recolor`, `restartPane`, `movePane`.
- Input surface (sharp edge): `sendInput(paneId, text)` / `sendKey` — see Safety.
- Events (WS): pane output stream, exit, focus change.
- Implementation: node-pty lives in `main` (`session.ts`/`session-manager.ts`), so
  read-output and send-input can be served **main-side** (tee the existing batched pty
  output; add `write(uid, data)`). Pane/tab/window *structure* is owned by the renderer
  store (`useWorkspace`); keep a lightweight read-model mirrored in `main` for queries,
  route structural mutations renderer-side via IPC.

## M3 — MCP project, Phase 1 (separate repo): launch-config

Stateless; depends only on the M1 CLI. No running-app dependency — ships first.

- Tools: `build_workspace(spec) → writes/returns JSON`, `launch_workspace(path|spec)` →
  spawns `hyperpanes`, `list_layouts`, `validate_workspace`.
- Essentially typed codegen + shell-out, with the `windows`/`groups`/`panes` schema as the
  contract.

## M4 — MCP project, Phase 2 (separate repo): live control

Thin adapter over the M2 control API.

- Tools: `list_panes`, `read_pane(tail)`, `open_pane`, `set_layout`, `close_pane`,
  `restart_pane`, `focus_pane`, `send_input` (guarded), plus pane output exposed as MCP
  **resources** with subscriptions for streaming.
- **Safety model for `send_input`** (an agent typing into live shells):
  - Control server bound to loopback + token; **disabled by default**, enabled via an
    explicit in-app toggle ("Allow agent control").
  - `send_input` gated behind an allowlist / per-call confirmation; never on by default.
  - Document the risk prominently in the MCP project README.

## Cross-cutting risks / notes

- Single-instance argv differs dev vs packaged; handle macOS `open-file` and Windows
  shortcut args.
- Per-window seeding interacts with autosave/restore — decide multi-window persistence
  scope (M0 note).
- Control-API security: loopback bind, token, explicit consent for input.
- Keep JSON canonical; the CLI grammar is sugar that compiles to the same `windows[]`
  structure — one seeding/validation path, not two.
