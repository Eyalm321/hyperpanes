import { app, dialog, type BrowserWindow } from 'electron';
import { existsSync, readFileSync, writeFileSync } from 'fs';
import { dirname, isAbsolute, join, resolve } from 'path';

export interface PaneSpec {
  label?: string;
  color?: string;
  command?: string;
  cwd?: string;
  shell?: string;
  fontSize?: number;
  meta?: Record<string, string>; // free-form per-pane metadata (agent-orchestration C)
}

export interface GroupSpec {
  title?: string;
  layout?: string;
  panes: PaneSpec[];
  sizes?: number[]; // per-slot split fractions (sum→1); length must match panes
  mainFraction?: number; // main-stack split fraction (0<f<1)
  focused?: number; // index of the focused pane (default 0)
  zoomed?: number; // index of the maximized pane (default: none)
}

export interface WindowBounds {
  x?: number;
  y?: number;
  width?: number;
  height?: number;
  maximized?: boolean;
  fullscreen?: boolean;
}

// One OS window: its tabs (groups), the active tab index, and optional bounds.
export interface WindowSpec {
  title?: string;
  active?: number;
  bounds?: WindowBounds;
  groups: GroupSpec[];
}

export interface WorkspaceFile {
  name?: string;
  layout?: string;
  panes?: PaneSpec[];
  groups?: GroupSpec[];
  active?: number;
  windows?: WindowSpec[];
}

// Relative cwds in a workspace file resolve against the file's own directory —
// applied to the top-level panes, every group's panes, and every window's
// groups' panes.
function resolveCwds(file: WorkspaceFile, baseDir: string): WorkspaceFile {
  const fixPanes = (panes: PaneSpec[]): PaneSpec[] =>
    panes.map((p) => ({
      ...p,
      cwd: p.cwd ? (isAbsolute(p.cwd) ? p.cwd : resolve(baseDir, p.cwd)) : undefined
    }));
  const fixGroups = (groups: GroupSpec[]): GroupSpec[] =>
    groups.map((g) => ({ ...g, panes: fixPanes(g.panes ?? []) }));
  return {
    ...file,
    ...(file.panes ? { panes: fixPanes(file.panes) } : {}),
    ...(file.groups ? { groups: fixGroups(file.groups) } : {}),
    ...(file.windows
      ? { windows: file.windows.map((w) => ({ ...w, groups: fixGroups(w.groups ?? []) })) }
      : {})
  };
}

// A file is loadable if it describes panes at any nesting level.
function hasPanes(file: WorkspaceFile | null): file is WorkspaceFile {
  return (
    !!file &&
    (Array.isArray(file.panes) || Array.isArray(file.groups) || Array.isArray(file.windows))
  );
}

export function readWorkspace(path: string): WorkspaceFile | null {
  try {
    const json = JSON.parse(readFileSync(path, 'utf8')) as WorkspaceFile;
    if (!hasPanes(json)) return null;
    return resolveCwds(json, dirname(path));
  } catch (err) {
    console.error('failed to read workspace', path, err);
    return null;
  }
}

/**
 * Normalize any workspace file into a flat list of windows — the one shape the
 * launcher seeds from. Precedence mirrors the schema's nesting:
 *   • `windows[]`            → used verbatim
 *   • `groups[]` (+ active)  → a single window holding those tabs
 *   • `panes[]`  (+ layout)  → a single window with one tab
 * Returns [] for null / contentless input (caller spawns a bare window).
 */
export function windowsOf(file: WorkspaceFile | null): WindowSpec[] {
  if (!file) return [];
  if (file.windows && file.windows.length > 0) {
    return file.windows.filter((w) => Array.isArray(w.groups) && w.groups.length > 0);
  }
  if (file.groups && file.groups.length > 0) {
    return [{ title: file.name, active: file.active, groups: file.groups }];
  }
  if (file.panes && file.panes.length > 0) {
    return [{ title: file.name, groups: [{ title: file.name, layout: file.layout, panes: file.panes }] }];
  }
  return [];
}

function writeWorkspace(path: string, data: WorkspaceFile): boolean {
  try {
    writeFileSync(path, JSON.stringify(data, null, 2), 'utf8');
    return true;
  } catch (err) {
    console.error('failed to write workspace', path, err);
    return false;
  }
}

export interface ParsedCli {
  /** A workspace assembled from inline flags (`-c`, `--layout`, …), or null. */
  workspace: WorkspaceFile | null;
  /** A positional `.json` path, e.g. `hyperpanes ./dev.json`. */
  jsonPath: string | null;
}

/**
 * Parse a launch command line into a workspace. Two input shapes, not mixed —
 * inline flags win if any `-c` is present:
 *
 *   hyperpanes ./dev.json
 *   hyperpanes --window --name app --layout main-stack \
 *                -c "npm run dev" -l server --color "#e5484d" --cwd ./app --shell pwsh \
 *                -c "tail -f log" -l logs --font 12 \
 *              --tab --name tests --layout columns -c vitest \
 *              --window --name db -c "psql mydb" --cwd ./db
 *
 * The flags form a window → tab → pane state machine:
 *   • `--window` opens a window, `--tab` opens a tab inside it (auto-created if
 *     omitted). A `--name` right after either titles that window/tab; a `--name`
 *     before any separator is the workspace name.
 *   • `-c`/`--command` adds a pane to the current tab. `-l`/`--label`, `--color`,
 *     `--cwd`, `--shell`, `--font` attach to the most recent `-c`.
 *   • `--cwd`/`--shell` seen before any `-c` are launch-wide defaults applied to
 *     every pane lacking its own; `--layout` sets the current (or next) tab.
 *
 * Output is back-compatible: with no `--window`/`--tab` it's the legacy
 * single-window `{ name, layout, panes }`; otherwise `{ name, windows }`. Both
 * normalize through windowsOf. Pure (touches the fs only to confirm a `.json`),
 * so it's unit-testable.
 */
interface CliTab {
  title?: string;
  layout?: string;
  panes: PaneSpec[];
}
interface CliWin {
  title?: string;
  tabs: CliTab[];
}

export function parseCli(argv: string[], existsFn: (p: string) => boolean = existsSync): ParsedCli {
  const args = argv.slice(1);
  const windows: CliWin[] = [];
  // The cursor lives in an object, not bare `let`s: TS won't track reassignments
  // made inside the helper closures below, so a `let cur = null` would stay
  // narrowed to `null` (and `if (cur)` would collapse to `never`). Object
  // properties re-narrow from their declared type at each access, so this works.
  const cur: { win: CliWin | null; tab: CliTab | null; pane: PaneSpec | null } = {
    win: null,
    tab: null,
    pane: null
  };
  // Which scope a following `--name` titles: the just-opened window or tab.
  let headerScope: 'window' | 'tab' | null = null;
  // `--layout` before a tab exists is held until the next tab is created.
  let pendingLayout: string | undefined;
  let explicitStructure = false; // any --window / --tab seen → emit the windows shape
  let name: string | undefined;
  let defaultCwd: string | undefined;
  let defaultShell: string | undefined;
  let jsonPath: string | null = null;

  const openWindow = () => {
    cur.win = { tabs: [] };
    windows.push(cur.win);
    cur.tab = null;
    cur.pane = null;
  };
  const openTab = () => {
    if (!cur.win) openWindow();
    const tab: CliTab = { panes: [] };
    if (pendingLayout) {
      tab.layout = pendingLayout;
      pendingLayout = undefined;
    }
    cur.win!.tabs.push(tab);
    cur.tab = tab;
    cur.pane = null;
  };
  const ensureTab = () => {
    if (!cur.tab) openTab();
  };

  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    const value = () => args[++i];
    switch (a) {
      case '--window':
        openWindow();
        headerScope = 'window';
        explicitStructure = true;
        break;
      case '--tab':
        openTab();
        headerScope = 'tab';
        explicitStructure = true;
        break;
      case '-c':
      case '--command':
        ensureTab();
        cur.pane = { command: value() };
        cur.tab!.panes.push(cur.pane);
        headerScope = null;
        break;
      case '-l':
      case '--label': {
        const v = value();
        if (cur.pane) cur.pane.label = v;
        break;
      }
      case '--color': {
        const v = value();
        if (cur.pane) cur.pane.color = v;
        break;
      }
      case '--cwd': {
        const v = value();
        if (cur.pane) cur.pane.cwd = v;
        else defaultCwd = v;
        break;
      }
      case '--shell': {
        const v = value();
        if (cur.pane) cur.pane.shell = v;
        else defaultShell = v;
        break;
      }
      case '--font': {
        const n = parseInt(value(), 10);
        if (cur.pane && Number.isFinite(n)) cur.pane.fontSize = n;
        break;
      }
      case '--layout': {
        const v = value();
        if (cur.tab) cur.tab.layout = v;
        else pendingLayout = v;
        break;
      }
      case '--name': {
        const v = value();
        if (headerScope === 'window' && cur.win) cur.win.title = v;
        else if (headerScope === 'tab' && cur.tab) cur.tab.title = v;
        else if (!explicitStructure) name = v;
        else if (cur.tab) cur.tab.title = v;
        else if (cur.win) cur.win.title = v;
        break;
      }
      default:
        if (!a.startsWith('-') && a.toLowerCase().endsWith('.json') && existsFn(a)) {
          jsonPath = resolve(a);
        }
    }
  }

  // Finish panes (label default + launch-wide cwd/shell), then prune empties.
  const allPanes = windows.flatMap((w) => w.tabs).flatMap((t) => t.panes);
  if (allPanes.length === 0) return { workspace: null, jsonPath };
  for (const p of allPanes) {
    if (!p.label && p.command) p.label = p.command.trim().split(/\s+/)[0] || 'shell';
    if (defaultCwd && !p.cwd) p.cwd = defaultCwd;
    if (defaultShell && !p.shell) p.shell = defaultShell;
  }
  const pruned = windows
    .map((w) => ({ ...w, tabs: w.tabs.filter((t) => t.panes.length > 0) }))
    .filter((w) => w.tabs.length > 0);

  if (!explicitStructure) {
    // Legacy single-window / single-tab shape.
    const tab = pruned[0].tabs[0];
    return { workspace: { name, layout: tab.layout, panes: tab.panes }, jsonPath };
  }
  const winSpecs: WindowSpec[] = pruned.map((w) => ({
    title: w.title,
    groups: w.tabs.map<GroupSpec>((t) => ({ title: t.title, layout: t.layout, panes: t.panes }))
  }));
  return { workspace: { name, windows: winSpecs }, jsonPath };
}

const lastPath = () => join(app.getPath('userData'), 'last-workspace.json');

export function writeLast(data: WorkspaceFile): void {
  writeWorkspace(lastPath(), data);
}

// What to load on launch: inline `-c` flags win, then an explicit `.json`, then
// the last session. Relative cwds resolve against the launch directory.
export function getInitialWorkspace(): WorkspaceFile | null {
  return resolveLaunchWorkspace(process.argv, process.cwd());
}

// The launch resolution behind getInitialWorkspace, parameterized by argv + cwd
// so it also serves the `second-instance` event (a second `hyperpanes …` while
// the app is already running — its argv/cwd, routed into this process).
export function resolveLaunchWorkspace(argv: string[], cwd: string): WorkspaceFile | null {
  const { workspace, jsonPath } = parseCli(argv);
  if (workspace) return resolveCwds(workspace, cwd);
  if (jsonPath) return readWorkspace(jsonPath);
  const last = lastPath();
  return existsSync(last) ? readWorkspace(last) : null;
}

// The window list to open on first launch (last-session restore included).
export function getInitialWindows(): WindowSpec[] {
  return windowsOf(getInitialWorkspace());
}

// The window list a second `hyperpanes …` invocation wants opened — its CLI/json
// only (no last-session fallback: a relaunch with no args should just focus, not
// reopen the saved session). Returns [] when the relaunch carried no content.
export function resolveSecondInstanceWindows(argv: string[], cwd: string): WindowSpec[] {
  const { workspace, jsonPath } = parseCli(argv);
  if (workspace) return windowsOf(resolveCwds(workspace, cwd));
  if (jsonPath) return windowsOf(readWorkspace(jsonPath));
  return [];
}

export async function openWorkspaceDialog(win: BrowserWindow | null): Promise<WorkspaceFile | null> {
  const opts = {
    title: 'Open workspace',
    filters: [{ name: 'Workspace', extensions: ['json'] }],
    properties: ['openFile'] as Array<'openFile'>
  };
  const res = win ? await dialog.showOpenDialog(win, opts) : await dialog.showOpenDialog(opts);
  if (res.canceled || !res.filePaths[0]) return null;
  return readWorkspace(res.filePaths[0]);
}

export async function saveWorkspaceDialog(
  win: BrowserWindow | null,
  data: WorkspaceFile
): Promise<boolean> {
  const opts = {
    title: 'Save workspace',
    defaultPath: `${data.name || 'workspace'}.json`,
    filters: [{ name: 'Workspace', extensions: ['json'] }]
  };
  const res = win ? await dialog.showSaveDialog(win, opts) : await dialog.showSaveDialog(opts);
  if (res.canceled || !res.filePath) return false;
  return writeWorkspace(res.filePath, data);
}
