// 'auto' tiles by pane count (see effectiveLayout); the rest are concrete presets.
export type Layout = 'auto' | 'single' | 'columns' | 'rows' | 'grid' | 'main-stack';

export type Direction = 'left' | 'right' | 'up' | 'down';

export interface Pane {
  id: string; // stable identity, survives session restart
  sessionUid: string; // current pty session (changes on restart)
  label: string; // user-facing name (locked in Phase 3)
  subtitle?: string; // optional secondary line shown under the label
  color: string; // frame color
  command?: string; // run via `shell -c`; absent = interactive shell
  cwd?: string;
  shell?: string; // pty shell override (e.g. 'pwsh'); absent = the default shell
  status: 'running' | 'exited';
  exitCode?: number;
  fontSize?: number; // per-pane terminal font size (absent = default)
}

// Persisted workspace file (workspace.json) — a declarative pane set + layout.
export interface PaneSpec {
  label?: string;
  subtitle?: string;
  color?: string;
  command?: string;
  cwd?: string;
  shell?: string;
  fontSize?: number;
}

// A single group (tab): a pane set + layout + tab title.
export interface GroupSpec {
  title?: string;
  layout?: Layout;
  panes: PaneSpec[];
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

// A workspace file holds one group at the top level (back-compat) and, for the
// full multi-tab session, the complete `groups` list with the active index.
export interface WorkspaceFile {
  name?: string;
  layout?: Layout;
  panes: PaneSpec[];
  groups?: GroupSpec[];
  active?: number;
}
