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
}

export interface GroupSpec {
  title?: string;
  layout?: string;
  panes: PaneSpec[];
}

export interface WorkspaceFile {
  name?: string;
  layout?: string;
  panes: PaneSpec[];
  groups?: GroupSpec[];
  active?: number;
}

// Relative cwds in a workspace file resolve against the file's own directory —
// applied to both the top-level panes and every group's panes.
function resolveCwds(file: WorkspaceFile, baseDir: string): WorkspaceFile {
  const fixPanes = (panes: PaneSpec[]): PaneSpec[] =>
    panes.map((p) => ({
      ...p,
      cwd: p.cwd ? (isAbsolute(p.cwd) ? p.cwd : resolve(baseDir, p.cwd)) : undefined
    }));
  return {
    ...file,
    panes: fixPanes(file.panes),
    ...(file.groups ? { groups: file.groups.map((g) => ({ ...g, panes: fixPanes(g.panes) })) } : {})
  };
}

export function readWorkspace(path: string): WorkspaceFile | null {
  try {
    const json = JSON.parse(readFileSync(path, 'utf8')) as WorkspaceFile;
    if (!json || !Array.isArray(json.panes)) return null;
    return resolveCwds(json, dirname(path));
  } catch (err) {
    console.error('failed to read workspace', path, err);
    return null;
  }
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
 * Parse a launch command line into a workspace. Two shapes are supported and
 * can't be mixed — inline flags win if any `-c` is present:
 *
 *   hyperpanes ./dev.json
 *   hyperpanes --shell pwsh -c "npm run dev" -l server -c "tail -f log" --layout main-stack
 *
 * Pure (takes argv + cwd, touches the fs only to confirm a `.json` exists) so
 * it can be unit-tested. `--label`/`--color` attach to the most recent `-c`;
 * `--cwd`/`--shell` are launch-wide defaults applied to every pane lacking one.
 */
export function parseCli(argv: string[], existsFn: (p: string) => boolean = existsSync): ParsedCli {
  const args = argv.slice(1);
  const panes: PaneSpec[] = [];
  let layout: string | undefined;
  let name: string | undefined;
  let defaultCwd: string | undefined;
  let defaultShell: string | undefined;
  let jsonPath: string | null = null;

  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    const value = () => args[++i];
    switch (a) {
      case '-c':
      case '--command':
        panes.push({ command: value() });
        break;
      case '-l':
      case '--label': {
        const v = value();
        if (panes.length) panes[panes.length - 1].label = v;
        break;
      }
      case '--color': {
        const v = value();
        if (panes.length) panes[panes.length - 1].color = v;
        break;
      }
      case '--cwd':
        defaultCwd = value();
        break;
      case '--shell':
        defaultShell = value();
        break;
      case '--layout':
        layout = value();
        break;
      case '--name':
        name = value();
        break;
      default:
        if (!a.startsWith('-') && a.toLowerCase().endsWith('.json') && existsFn(a)) {
          jsonPath = resolve(a);
        }
    }
  }

  if (panes.length === 0) return { workspace: null, jsonPath };

  for (const p of panes) {
    // A pane's label defaults to the first word of its command.
    if (!p.label && p.command) p.label = p.command.trim().split(/\s+/)[0] || 'shell';
    if (defaultCwd && !p.cwd) p.cwd = defaultCwd;
    if (defaultShell && !p.shell) p.shell = defaultShell;
  }
  return { workspace: { name, layout, panes }, jsonPath };
}

const lastPath = () => join(app.getPath('userData'), 'last-workspace.json');

export function writeLast(data: WorkspaceFile): void {
  writeWorkspace(lastPath(), data);
}

// What to load on launch: inline `-c` flags win, then an explicit `.json`, then
// the last session. Relative cwds resolve against the launch directory.
export function getInitialWorkspace(): WorkspaceFile | null {
  const { workspace, jsonPath } = parseCli(process.argv);
  if (workspace) return resolveCwds(workspace, process.cwd());
  if (jsonPath) return readWorkspace(jsonPath);
  const last = lastPath();
  return existsSync(last) ? readWorkspace(last) : null;
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
