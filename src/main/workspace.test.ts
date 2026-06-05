import { describe, it, expect, vi } from 'vitest';

// workspace.ts imports `electron` at module load; stub it so the pure parser
// can be tested under plain Node.
vi.mock('electron', () => ({ app: {}, dialog: {} }));

import { parseCli } from './workspace';

const argv = (...rest: string[]) => ['/path/to/hyperpanes', ...rest];

describe('parseCli', () => {
  it('returns nothing for a bare launch', () => {
    expect(parseCli(argv())).toEqual({ workspace: null, jsonPath: null });
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
});
