import { describe, it, expect, vi } from 'vitest';

// workspace.ts imports `electron` at module load; stub it so the pure parser
// can be tested under plain Node.
vi.mock('electron', () => ({ app: {}, dialog: {} }));

import { parseCli, windowsOf } from './workspace';

const argv = (...rest: string[]) => ['/path/to/hyperpanes', ...rest];

describe('parseCli', () => {
  it('returns nothing for a bare launch', () => {
    expect(parseCli(argv())).toEqual({
      workspace: null,
      jsonPath: null,
      routing: { mode: 'attach', target: 'focused', as: 'tab' }
    });
  });

  it('builds panes from repeated -c flags', () => {
    const { workspace } = parseCli(argv('-c', 'npm run dev', '-c', 'tail -f log'));
    expect(workspace?.panes).toEqual([
      { command: 'npm run dev', label: 'npm' },
      { command: 'tail -f log', label: 'tail' }
    ]);
  });

  it('attaches --label and --color to the most recent command', () => {
    const { workspace } = parseCli(
      argv('-c', 'npm run dev', '-l', 'server', '--color', '#e5484d', '-c', 'psql', '--label', 'db')
    );
    expect(workspace?.panes).toEqual([
      { command: 'npm run dev', label: 'server', color: '#e5484d' },
      { command: 'psql', label: 'db' }
    ]);
  });

  it('reads --layout, --name and applies --cwd to panes without one', () => {
    const { workspace } = parseCli(
      argv('--name', 'dev', '--layout', 'main-stack', '--cwd', '/work', '-c', 'bash')
    );
    expect(workspace).toEqual({
      name: 'dev',
      layout: 'main-stack',
      panes: [{ command: 'bash', label: 'bash', cwd: '/work' }]
    });
  });

  it('applies --shell as a launch-wide default to panes without one', () => {
    const { workspace } = parseCli(argv('--shell', 'pwsh', '-c', 'npm run dev', '-c', 'top'));
    expect(workspace?.panes).toEqual([
      { command: 'npm run dev', label: 'npm', shell: 'pwsh' },
      { command: 'top', label: 'top', shell: 'pwsh' }
    ]);
  });

  it('captures a positional .json path that exists', () => {
    const exists = (p: string) => p === './dev.json';
    const { jsonPath, workspace } = parseCli(argv('./dev.json'), exists);
    expect(workspace).toBeNull();
    expect(jsonPath).toMatch(/dev\.json$/);
  });

  it('ignores a .json path that does not exist', () => {
    const { jsonPath } = parseCli(argv('./missing.json'), () => false);
    expect(jsonPath).toBeNull();
  });

  // ---- M1: per-pane flags + --tab / --window separators ----

  it('attaches per-pane --cwd / --shell / --font to the most recent -c', () => {
    const { workspace } = parseCli(
      argv('-c', 'npm run dev', '--cwd', '/app', '--shell', 'pwsh', '--font', '14', '-c', 'top')
    );
    // No separators → legacy single-window shape, panes carry their own settings.
    expect(workspace?.panes).toEqual([
      { command: 'npm run dev', label: 'npm', cwd: '/app', shell: 'pwsh', fontSize: 14 },
      { command: 'top', label: 'top' }
    ]);
  });

  it('keeps --cwd / --shell before any -c as launch-wide defaults', () => {
    const { workspace } = parseCli(argv('--cwd', '/work', '-c', 'a', '-c', 'b', '--cwd', '/b'));
    expect(workspace?.panes).toEqual([
      { command: 'a', label: 'a', cwd: '/work' },
      { command: 'b', label: 'b', cwd: '/b' } // a per-pane --cwd overrides the default
    ]);
  });

  it('builds multiple tabs in one window with --tab', () => {
    const { workspace } = parseCli(
      argv('--tab', '--name', 'app', '-c', 'a', '--tab', '--name', 'logs', '-c', 'b')
    );
    expect(workspace?.windows).toEqual([
      {
        title: undefined,
        groups: [
          { title: 'app', layout: undefined, panes: [{ command: 'a', label: 'a' }] },
          { title: 'logs', layout: undefined, panes: [{ command: 'b', label: 'b' }] }
        ]
      }
    ]);
  });

  it('builds multiple windows with --window, titling each', () => {
    const { workspace } = parseCli(
      argv('--window', '--name', 'one', '--layout', 'grid', '-c', 'a', '--window', '--name', 'two', '-c', 'b')
    );
    expect(workspace?.windows).toEqual([
      { title: 'one', groups: [{ title: undefined, layout: 'grid', panes: [{ command: 'a', label: 'a' }] }] },
      { title: 'two', groups: [{ title: undefined, layout: undefined, panes: [{ command: 'b', label: 'b' }] }] }
    ]);
    expect(workspace?.panes).toBeUndefined(); // windows shape, not legacy
  });

  it('drops --window / --tab that never got a pane', () => {
    const { workspace } = parseCli(argv('--window', '--tab', '--window', '-c', 'only'));
    expect(workspace?.windows).toEqual([
      { title: undefined, groups: [{ title: undefined, layout: undefined, panes: [{ command: 'only', label: 'only' }] }] }
    ]);
  });
});

// ---- launch routing: new window vs attach into an existing one ----
describe('parseCli routing', () => {
  it('defaults a bare/legacy launch to attach the focused window as a tab', () => {
    const { routing } = parseCli(argv('-c', 'npm run dev'));
    expect(routing).toEqual({ mode: 'attach', target: 'focused', as: 'tab' });
  });

  it('defaults a --tab-only launch (single window) to attach', () => {
    const { routing } = parseCli(argv('--tab', '-c', 'a', '--tab', '-c', 'b'));
    expect(routing).toEqual({ mode: 'attach', target: 'focused', as: 'tab' });
  });

  it('treats a --window separator as new-window intent by default', () => {
    const { routing } = parseCli(argv('--window', '-c', 'a'));
    expect(routing).toEqual({ mode: 'new-window' });
  });

  it('honors an explicit --new-window flag', () => {
    const { routing } = parseCli(argv('--new-window', '-c', 'a'));
    expect(routing).toEqual({ mode: 'new-window' });
  });

  it('--attach forces attach even when a --window separator is present', () => {
    const { routing } = parseCli(argv('--attach', '--window', '-c', 'a'));
    expect(routing).toEqual({ mode: 'attach', target: 'focused', as: 'tab' });
  });

  it('parses --attach=last and --attach=<id> targets', () => {
    expect(parseCli(argv('--attach=last', '-c', 'a')).routing).toEqual({
      mode: 'attach',
      target: 'last',
      as: 'tab'
    });
    expect(parseCli(argv('--attach=3', '-c', 'a')).routing).toEqual({
      mode: 'attach',
      target: 3,
      as: 'tab'
    });
  });

  it('--as panes implies attach and sets the unit', () => {
    const { routing } = parseCli(argv('--as', 'panes', '-c', 'a'));
    expect(routing).toEqual({ mode: 'attach', target: 'focused', as: 'panes' });
  });

  it('treats --into-current as attach to the focused window', () => {
    const { routing } = parseCli(argv('--into-current', '-c', 'a'));
    expect(routing).toEqual({ mode: 'attach', target: 'focused', as: 'tab' });
  });

  it('does not let a routing flag leak into the parsed panes', () => {
    const { workspace } = parseCli(argv('--new-window', '--as', 'panes', '-c', 'npm run dev'));
    expect(workspace?.panes).toEqual([{ command: 'npm run dev', label: 'npm' }]);
  });
});

describe('windowsOf', () => {
  it('returns [] for null / contentless', () => {
    expect(windowsOf(null)).toEqual([]);
    expect(windowsOf({})).toEqual([]);
    expect(windowsOf({ panes: [] })).toEqual([]);
  });

  it('wraps top-level panes as one window with one tab', () => {
    expect(windowsOf({ name: 'x', layout: 'grid', panes: [{ label: 'a' }] })).toEqual([
      { title: 'x', groups: [{ title: 'x', layout: 'grid', panes: [{ label: 'a' }] }] }
    ]);
  });

  it('wraps groups as one window of tabs, carrying active', () => {
    const groups = [{ title: 't1', panes: [{ label: 'a' }] }];
    expect(windowsOf({ name: 'x', groups, active: 0 })).toEqual([
      { title: 'x', active: 0, groups }
    ]);
  });

  it('uses windows verbatim, dropping groupless windows', () => {
    const win = { title: 'w', groups: [{ panes: [{ label: 'a' }] }] };
    expect(windowsOf({ windows: [win, { title: 'empty', groups: [] }] })).toEqual([win]);
  });
});
