import { describe, expect, it } from 'vitest';
import { activeGroup, useWorkspace } from '../store/useWorkspace';
import { serializeWorkspace } from './serialize';

describe('workspace serialize/load round-trip', () => {
  it('preserves name, layout and pane specs', () => {
    useWorkspace.getState().loadWorkspace({
      name: 'demo',
      layout: 'grid',
      panes: [
        { label: 'a', color: '#ffffff', command: 'ls', cwd: '/x' },
        { label: 'b', color: '#000000' }
      ]
    });

    const out = serializeWorkspace();
    expect(out.name).toBe('demo');
    expect(out.layout).toBe('grid');
    expect(out.panes).toEqual([
      { label: 'a', color: '#ffffff', command: 'ls', cwd: '/x' },
      { label: 'b', color: '#000000' }
    ]);
  });

  it('falls back to auto for an unknown layout', () => {
    useWorkspace.getState().loadWorkspace({ layout: 'bogus' as never, panes: [{ label: 'x' }] });
    expect(activeGroup(useWorkspace.getState()).layout).toBe('auto');
  });

  it('preserves an explicit saved layout without rewriting it to auto', () => {
    useWorkspace.getState().loadWorkspace({ layout: 'columns', panes: [{ label: 'x' }] });
    expect(activeGroup(useWorkspace.getState()).layout).toBe('columns');
  });

  it('round-trips a pane subtitle and omits it when absent', () => {
    useWorkspace.getState().loadWorkspace({
      name: 'demo',
      layout: 'grid',
      panes: [
        { label: 'a', color: '#e5484d', subtitle: 'feature/x' },
        { label: 'b', color: '#3b82f6' }
      ]
    });

    const out = serializeWorkspace();
    expect(out.panes[0]).toMatchObject({ label: 'a', subtitle: 'feature/x', color: '#e5484d' });
    // An absent subtitle is omitted, not written as an empty/undefined key.
    expect('subtitle' in out.panes[1]).toBe(false);
  });
});
