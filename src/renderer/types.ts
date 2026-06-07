// 'auto' tiles by pane count (see effectiveLayout); the rest are concrete presets.
export type Layout = 'auto' | 'single' | 'columns' | 'rows' | 'grid' | 'main-stack';

export type Direction = 'left' | 'right' | 'up' | 'down';

export interface Pane {
  id: string; // stable identity, survives session restart
  sessionUid: string; // current pty session (changes on restart)
  label: string; // user-facing name (locked in Phase 3)
  subtitle?: string; // optional secondary line shown under the label
  color: string; // frame color
  // Per-pane override of the global show-frame / show-dot Appearance toggles
  // (undefined = inherit useSettings). New panes default both to false (clean,
  // uncolored); opening a project or a git-project tint flips them on.
  showFrame?: boolean;
  showDot?: boolean;
  command?: string; // run via `shell -c`; absent = interactive shell
  // Literal argv for a DIRECT spawn (no shell, no re-parse): with `command` set,
  // runs `command` with exactly these args, so values containing spaces/quotes
  // survive intact (interactive-pane-driving plan P4a). Absent = the shell path.
  args?: string[];
  cwd?: string;
  shell?: string; // pty shell override (e.g. 'pwsh'); absent = the default shell
  status: 'running' | 'exited';
  exitCode?: number;
  fontSize?: number; // per-pane terminal font size (absent = default)
  // Free-form per-pane metadata (agent-orchestration C). Reserved keys give an
  // agent org its shape: `role`, `parent`, `agentType`, `task` — rest is open.
  meta?: Record<string, string>;
  // Extra env injected into this pane's pty at spawn (agent-orchestration F):
  // how a parent hands a child a scoped control token. Runtime-only — never
  // persisted (would leak tokens) and never published to the control plane.
  env?: Record<string, string>;
}

// Persisted workspace file (workspace.json) — a declarative pane set + layout.
export interface PaneSpec {
  label?: string;
  subtitle?: string;
  color?: string;
  showFrame?: boolean;
  showDot?: boolean;
  command?: string;
  args?: string[]; // literal argv for a direct (no-shell) spawn with `command` (P4a)
  cwd?: string;
  shell?: string;
  fontSize?: number;
  meta?: Record<string, string>; // free-form per-pane metadata (agent-orchestration C)
}

// ---- Git projects (sidebar projects history) ----
// A git repo the app remembers from a pane cd-ing into it. Persisted to
// projects.json by main. Each gets its own frame/dot color and is titled by the
// repo folder name; opening one spawns a pane cd'd into `path`, tinted `color`.
export interface Project {
  id: string;
  path: string; // normalized git-root absolute path
  name: string; // basename of the git root (the title)
  color: string; // frame/dot color for panes in this project
  lastOpenedAt?: number; // epoch ms; set by main, for recency sorting
}

// A single group (tab): a pane set + layout + tab title. The optional sizing /
// focus / zoom fields let a launched or restored tab reproduce its exact split
// ratios and which pane is focused / maximized; omit them for the defaults
// (equal split, first pane focused, none maximized).
export interface GroupSpec {
  title?: string;
  layout?: Layout;
  panes: PaneSpec[];
  sizes?: number[]; // per-slot fractions (summed→1); length must match panes
  mainFraction?: number; // main-stack split fraction (0<f<1)
  focused?: number; // index of the focused pane (default 0)
  zoomed?: number; // index of the maximized pane (default: none)
}

// One OS window: a set of tabs (groups), which one is active, and optional
// on-screen bounds. The `windows` layer sits above tabs so a single workspace
// file / CLI launch can describe several windows at once.
export interface WindowBounds {
  x?: number;
  y?: number;
  width?: number;
  height?: number;
  maximized?: boolean;
  fullscreen?: boolean;
}

export interface WindowSpec {
  title?: string;
  active?: number; // active tab index
  bounds?: WindowBounds;
  groups: GroupSpec[];
}

// A live group handed to another window during tear-off / merge (Stage 2). It
// mirrors the store's `Group` shape and is fully serializable, so it survives
// the IPC structured clone. Crucially the panes carry their *live* sessionUids,
// so the receiving window re-attaches to the running ptys instead of respawning.
export interface GroupPayload {
  id: string;
  title: string;
  panes: Pane[];
  layout: Layout;
  focusedId: string | null;
  zoomedId: string | null;
  sizes: number[];
  mainFraction: number;
  seq: number;
}

// ---- Control API (M2) ----
// What a window publishes to main about its own structure when the control API is
// active, plus the status main reports back to Preferences. Kept structurally
// identical to the main-side copies in control-server.ts.
export interface ControlPaneInfo {
  id: string;
  sessionUid: string;
  label: string;
  subtitle?: string; // secondary header line; omitted when unset (rename_pane sets it)
  color: string;
  command?: string;
  args?: string[]; // direct-spawn argv, if this pane was opened with one (P4a)
  cwd?: string;
  shell?: string;
  status: 'running' | 'exited';
  exitCode?: number;
  activity: 'busy' | 'idle' | 'exited'; // liveness heuristic (agent-orchestration B)
  meta?: Record<string, string>; // free-form per-pane metadata (agent-orchestration C)
}
export interface ControlTabInfo {
  id: string;
  title: string;
  layout: string;
  panes: ControlPaneInfo[];
}
export interface ControlWindowPayload {
  activeTabId: string | null;
  tabs: ControlTabInfo[];
}
export interface ControlStatus {
  enabled: boolean;
  allowInput: boolean;
  running: boolean;
  port: number | null;
}
// A structural command forwarded from the control API into a window's renderer.
export interface ControlCommand {
  type: string;
  paneId?: string;
  windowId?: number;
  correlationId?: string; // set by main; the renderer echoes it with the result (D)
  [key: string]: unknown;
}

// ---- Performance metrics (diagnostics) ----
// A point-in-time snapshot for the "Performance: Dump metrics" command: cold-start
// milestones (main-process), plus per-process memory from app.getAppMetrics(). The
// live WebGL-context count is added renderer-side (it's per-window, not in main).
export interface MetricsProcessInfo {
  type: string; // 'Browser' | 'GPU' | 'Tab' (renderer) | 'Utility' | …
  pid: number;
  memoryMB: number; // working-set size
  cpu: number; // percentCPUUsage since the last sample
}
export interface MetricsSnapshot {
  startupMs: Record<string, number>; // named cold-start marks (ms since process start)
  windows: number;
  totalMemoryMB: number;
  byType: { type: string; count: number; memoryMB: number }[];
  processes: MetricsProcessInfo[];
}

// A workspace file. Three nesting levels, each a back-compatible superset of the
// last:
//   • `panes` (+ `layout`)      — one tab in one window (original format)
//   • `groups` (+ `active`)     — several tabs in one window
//   • `windows`                 — several windows, each with its own tabs
// A reader normalizes whichever is present into a window list (see windowsOf in
// src/main/workspace.ts). `panes` is optional now that a file may be windows-only.
export interface WorkspaceFile {
  name?: string;
  layout?: Layout;
  panes?: PaneSpec[];
  groups?: GroupSpec[];
  active?: number;
  windows?: WindowSpec[];
}
