import { contextBridge, ipcRenderer } from 'electron';
import type {
  ControlCommand,
  ControlStatus,
  ControlWindowPayload,
  GroupPayload,
  MetricsSnapshot,
  Project,
  WindowSpec
} from '../renderer/types';

// What main reports when a window asks for its launch seed.
export interface SeedInfo {
  seed: GroupPayload | null; // a live group torn off into this (new) window, if any
  windowSpec?: WindowSpec | null; // a launch-time window spec (tabs to materialize), if any
  primary: boolean; // the session-of-record window (owns last-workspace.json)
}

export interface SpawnOptions {
  uid: string;
  paneId?: string; // forwarded to main; injected into the pty env as HYPERPANES_PANE_ID
  shell?: string;
  command?: string;
  args?: string[]; // direct-spawn argv (with command): verbatim, no shell re-parse (P4a)
  cwd?: string;
  env?: Record<string, string>; // extra pty env (e.g. a scoped control token, agent-orchestration F)
  cols?: number;
  rows?: number;
}

type PaneSpec = {
  label?: string;
  subtitle?: string;
  color?: string;
  command?: string;
  args?: string[];
  cwd?: string;
  shell?: string;
  fontSize?: number;
  meta?: Record<string, string>;
};

type GroupSpec = { title?: string; layout?: string; panes: PaneSpec[] };

export interface WorkspaceFile {
  name?: string;
  layout?: string;
  panes?: PaneSpec[];
  groups?: GroupSpec[];
  active?: number;
  windows?: Array<{ title?: string; active?: number; bounds?: unknown; groups: GroupSpec[] }>;
}

// One ipcRenderer listener per channel, fanned out to N per-pane subscribers, so
// the underlying listener count stays at 1 regardless of how many panes mount.
// Each pane used to add its own ipcRenderer.on('session:data'/'session:exit'),
// and because background tabs keep their shells alive, >10 live panes tripped
// Node's default MaxListeners=10 with a false-positive "possible EventEmitter
// memory leak" warning. Subscribers still filter by uid themselves (see
// Terminal.tsx), so dispatch stays O(N) — only the listener count is fixed.
const dataSubs = new Set<(uid: string, data: string) => void>();
const exitSubs = new Set<(uid: string, code: number) => void>();
let dataWired = false;
let exitWired = false;

const api = {
  // The host platform ('win32' | 'darwin' | 'linux' | …), so the UI can offer
  // platform-appropriate shell presets without another round-trip.
  platform: process.platform,

  spawn: (opts: SpawnOptions): Promise<{ uid: string; attached: boolean; replay?: string }> =>
    ipcRenderer.invoke('session:spawn', opts),

  write: (uid: string, data: string): void =>
    ipcRenderer.send('session:write', { uid, data }),

  resize: (uid: string, cols: number, rows: number): void =>
    ipcRenderer.send('session:resize', { uid, cols, rows }),

  kill: (uid: string): void => ipcRenderer.send('session:kill', { uid }),

  // Diagnostics: a memory/process/startup snapshot for "Performance: Dump metrics".
  metrics: (): Promise<MetricsSnapshot> => ipcRenderer.invoke('metrics:get'),

  // Clickable file paths: verify candidate tokens against the pane's cwd, and
  // open a verified path in the configured editor / OS default handler.
  paths: {
    resolve: (
      cwd: string | undefined,
      tokens: string[]
    ): Promise<
      { token: string; absPath: string; exists: boolean; isDir: boolean; isExe: boolean }[]
    > => ipcRenderer.invoke('path:resolve', { cwd, tokens }),
    open: (
      absPath: string,
      line: number | undefined,
      col: number | undefined,
      editorCommand: string
    ): Promise<{ ok: boolean; blocked?: boolean; error?: string }> =>
      ipcRenderer.invoke('path:open', { absPath, line, col, editorCommand })
  },

  onData: (cb: (uid: string, data: string) => void): (() => void) => {
    if (!dataWired) {
      ipcRenderer.on('session:data', (_e, p: { uid: string; data: string }) => {
        for (const sub of dataSubs) sub(p.uid, p.data);
      });
      dataWired = true;
    }
    dataSubs.add(cb);
    return () => {
      dataSubs.delete(cb);
    };
  },

  onExit: (cb: (uid: string, code: number) => void): (() => void) => {
    if (!exitWired) {
      ipcRenderer.on('session:exit', (_e, p: { uid: string; code: number }) => {
        for (const sub of exitSubs) sub(p.uid, p.code);
      });
      exitWired = true;
    }
    exitSubs.add(cb);
    return () => {
      exitSubs.delete(cb);
    };
  },

  // Live cwd from OSC 7 shell integration (one global subscription is plenty).
  onCwd: (cb: (uid: string, cwd: string) => void): (() => void) => {
    const listener = (_e: unknown, p: { uid: string; cwd: string }) => cb(p.uid, p.cwd);
    ipcRenderer.on('session:cwd', listener);
    return () => ipcRenderer.removeListener('session:cwd', listener);
  },

  // Git projects (sidebar projects history). `onPaneProject` fires when a pane's
  // cwd enters a known git root, so the renderer can tint that pane.
  projects: {
    list: (): Promise<Project[]> => ipcRenderer.invoke('projects:list'),
    setColor: (id: string, color: string): Promise<Project[]> =>
      ipcRenderer.invoke('projects:setColor', { id, color }),
    rename: (id: string, name: string): Promise<Project[]> =>
      ipcRenderer.invoke('projects:rename', { id, name }),
    remove: (id: string): Promise<Project[]> => ipcRenderer.invoke('projects:remove', { id }),
    onChanged: (cb: (list: Project[]) => void): (() => void) => {
      const listener = (_e: unknown, list: Project[]) => cb(list);
      ipcRenderer.on('projects:changed', listener);
      return () => ipcRenderer.removeListener('projects:changed', listener);
    },
    onPaneProject: (cb: (uid: string, project: Project) => void): (() => void) => {
      const listener = (_e: unknown, p: { uid: string; project: Project }) => cb(p.uid, p.project);
      ipcRenderer.on('session:project', listener);
      return () => ipcRenderer.removeListener('session:project', listener);
    }
  },

  workspace: {
    getInitial: (): Promise<WorkspaceFile | null> => ipcRenderer.invoke('workspace:getInitial'),
    open: (): Promise<WorkspaceFile | null> => ipcRenderer.invoke('workspace:open'),
    save: (data: WorkspaceFile): Promise<boolean> => ipcRenderer.invoke('workspace:save', data),
    // Per-window session snapshot for multi-window autosave; main aggregates by
    // window and writes the combined `windows[]` last session.
    publishSession: (payload: { active: number; groups: GroupSpec[] }): void =>
      ipcRenderer.send('workspace:windowSession', payload)
  },

  // Local control API (M2). Preferences reads/sets the toggles; while active each
  // window publishes its structure and listens for forwarded commands.
  control: {
    getStatus: (): Promise<ControlStatus> => ipcRenderer.invoke('control:getStatus'),
    setEnabled: (enabled: boolean): Promise<ControlStatus> =>
      ipcRenderer.invoke('control:setEnabled', enabled),
    setAllowInput: (allow: boolean): Promise<ControlStatus> =>
      ipcRenderer.invoke('control:setAllowInput', allow),
    publishState: (payload: ControlWindowPayload): void =>
      ipcRenderer.send('control:publishState', payload),
    onActive: (cb: (active: boolean) => void): (() => void) => {
      const listener = (_e: unknown, active: boolean) => cb(active);
      ipcRenderer.on('control:active', listener);
      return () => ipcRenderer.removeListener('control:active', listener);
    },
    onCommand: (cb: (command: ControlCommand) => void): (() => void) => {
      const listener = (_e: unknown, command: ControlCommand) => cb(command);
      ipcRenderer.on('control:command', listener);
      return () => ipcRenderer.removeListener('control:command', listener);
    },
    // Reply to a dispatched command (matched by correlationId) with its outcome —
    // `{ ok:true, result? }` or `{ ok:false, error }` — so main can resolve the
    // awaiting /command HTTP response with the right status (D, #2/#3).
    commandResult: (
      correlationId: string,
      reply: { ok: boolean; result?: unknown; error?: string }
    ): void => ipcRenderer.send('control:commandResult', { correlationId, ...reply })
  },

  win: {
    minimize: (): void => ipcRenderer.send('window:minimize'),
    toggleMaximize: (): void => ipcRenderer.send('window:toggleMaximize'),
    close: (): void => ipcRenderer.send('window:close'),
    isMaximized: (): Promise<boolean> => ipcRenderer.invoke('window:isMaximized'),
    onMaximizeChange: (cb: (maximized: boolean) => void): (() => void) => {
      const listener = (_e: unknown, maximized: boolean) => cb(maximized);
      ipcRenderer.on('window:maximized', listener);
      return () => ipcRenderer.removeListener('window:maximized', listener);
    },

    // Pane fullscreen: drive OS fullscreen, and hear back when it flips (incl.
    // native exits) so the renderer can sync its pane-fullscreen state.
    setFullScreen: (on: boolean): void => ipcRenderer.send('window:setFullScreen', on),
    onFullScreenChange: (cb: (fullscreen: boolean) => void): (() => void) => {
      const listener = (_e: unknown, fullscreen: boolean) => cb(fullscreen);
      ipcRenderer.on('window:fullscreen', listener);
      return () => ipcRenderer.removeListener('window:fullscreen', listener);
    },

    // Pulled once on mount: the group this window was torn off with (or none),
    // plus whether this is the session-of-record window. Pull (not push) so
    // there's no race against the renderer registering a listener.
    getSeed: (): Promise<SeedInfo> => ipcRenderer.invoke('window:getSeed'),

    // Move to New Window (non-drag): open a fresh window seeded with an already-
    // extracted group, near the cursor. The group's sessions were flagged "moving"
    // by the renderer, so the new window re-attaches to the live ptys.
    spawnGroupWindow: (group: GroupPayload): Promise<{ ok: boolean }> =>
      ipcRenderer.invoke('window:spawnGroup', group),

    // Live tear-off: hand main the extracted group; it opens a real window under
    // the cursor that follows it and docks into another window's tab bar like
    // Chrome. `dragDrop` (pointer released) and `dragCancel` (pointer cancelled)
    // end the drag — main docks it into the previewed strip or settles the float.
    // `moveWindow` = the dragged tab/pane is the source window's entire content
    // (its only tab / sole pane), so main drags THIS window instead of spawning a
    // seeded copy — no duplicate window left behind.
    dragDetach: (group: GroupPayload, moveWindow?: boolean): Promise<{ id: number }> =>
      ipcRenderer.invoke('drag:detach', group, moveWindow),
    dragDrop: (): Promise<{ action: 'docked' | 'stitched' | 'detached' | 'none' }> =>
      ipcRenderer.invoke('drag:drop'),
    dragCancel: (): Promise<{ action: 'docked' | 'stitched' | 'detached' | 'none' }> =>
      ipcRenderer.invoke('drag:cancel'),

    // While a single-pane float hovers this window's body, main feeds us the cursor
    // (onPaneStitchPreview) and we report back whether it's near a pane edge (a real
    // slot). Main uses it to decide whether to reveal the dropline (hide the float)
    // or keep the float detached over the pane's dead centre. One-way for low cost.
    reportStitchHit: (valid: boolean): void => ipcRenderer.send('drag:stitchHit', valid),

    // A group docked into this window (from another window's tab being dragged
    // onto its strip). `x` is the window-relative cursor x of the drop slot.
    onReceiveTab: (cb: (group: GroupPayload, x?: number) => void): (() => void) => {
      const listener = (_e: unknown, p: { group: GroupPayload; x?: number }) => cb(p.group, p.x);
      ipcRenderer.on('tab:receive', listener);
      return () => ipcRenderer.removeListener('tab:receive', listener);
    },

    // Mid-drag dock preview: a tab being dragged is hovering this window's strip
    // at window-relative x (or `null` once it leaves). Drives the ghost slot.
    onTabPreview: (
      cb: (preview: { x: number; title: string } | null) => void
    ): (() => void) => {
      const listener = (_e: unknown, preview: { x: number; title: string } | null) => cb(preview);
      ipcRenderer.on('tab:preview', listener);
      return () => ipcRenderer.removeListener('tab:preview', listener);
    },

    // Cross-window pane stitch: a single-pane float from another window is hovering
    // THIS window's pane area at window-relative (x,y) — show the insert indicator
    // (or clear it on `null`). `onPaneStitch` commits: adopt the pane at that slot.
    onPaneStitchPreview: (
      cb: (at: { x: number; y: number } | null) => void
    ): (() => void) => {
      const listener = (_e: unknown, at: { x: number; y: number } | null) => cb(at);
      ipcRenderer.on('pane:preview', listener);
      return () => ipcRenderer.removeListener('pane:preview', listener);
    },
    onPaneStitch: (
      cb: (p: { group: GroupPayload; x: number; y: number }) => void
    ): (() => void) => {
      const listener = (_e: unknown, p: { group: GroupPayload; x: number; y: number }) => cb(p);
      ipcRenderer.on('pane:stitch', listener);
      return () => ipcRenderer.removeListener('pane:stitch', listener);
    },

    // This window was promoted to session-of-record (the prior primary closed).
    onPrimary: (cb: () => void): (() => void) => {
      const listener = () => cb();
      ipcRenderer.on('window:primary', listener);
      return () => ipcRenderer.removeListener('window:primary', listener);
    }
  }
};

contextBridge.exposeInMainWorld('hp', api);

export type HpApi = typeof api;
