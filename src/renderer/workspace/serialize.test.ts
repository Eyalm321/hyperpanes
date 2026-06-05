import { describe, expect, it } from 'vitest';
import { activeGroup, useWorkspace } from '../store/useWorkspace';
import { serializeWindowSession, serializeWorkspace } from './serialize';

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
    const panes = out.panes ?? [];
    expect(panes[0]).toMatchObject({ label: 'a', subtitle: 'feature/x', color: '#e5484d' });
    // An absent subtitle is omitted, not written as an empty/undefined key.
    expect('subtitle' in panes[1]).toBe(false);
  });

  it('round-trips pane meta and omits it when absent/empty (agent-orchestration C)', () => {
    useWorkspace.getState().loadSession({
      groups: [
        {
          layout: 'columns',
          panes: [
            { label: 'a', meta: { role: 'manager:frontend', parent: 'root' } },
            { label: 'b' }
          ]
        }
      ],
      active: 0
    });

    const out = serializeWindowSession();
    expect(out.groups[0].panes[0].meta).toEqual({ role: 'manager:frontend', parent: 'root' });
    // No meta → the key is omitted (not written as empty/undefined).
    expect('meta' in out.groups[0].panes[1]).toBe(false);
  });
});

describe('serializeWindowSession (per-window autosave)', () => {
  it('captures every tab and the active index for this window', () => {
    useWorkspace.getState().loadSession({
      groups: [
        { title: 't1', layout: 'columns', panes: [{ label: 'a' }] },
        { title: 't2', layout: 'grid', panes: [{ label: 'b' }, { label: 'c' }] }
      ],
      active: 1
    });

    const out = serializeWindowSession();
    expect(out.active).toBe(1);
    expect(out.groups.map((g) => g.title)).toEqual(['t1', 't2']);
    expect(out.groups[1].panes.map((p) => p.label)).toEqual(['b', 'c']);
  });
});

describe('GroupSpec sizing / focus / zoom round-trip', () => {
  it('applies and re-emits custom sizes, mainFraction, focused, zoomed', () => {
    useWorkspace.getState().loadSession({
      groups: [
        {
          layout: 'main-stack',
          panes: [{ label: 'a' }, { label: 'b' }, { label: 'c' }],
          sizes: [2, 1, 1], // normalized to fractions on load
          mainFraction: 0.7,
          focused: 2,
          zoomed: 1
        }
      ],
      active: 0
    });

    const g = activeGroup(useWorkspace.getState());
    expect(g.sizes).toEqual([0.5, 0.25, 0.25]);
    expect(g.mainFraction).toBe(0.7);
    expect(g.focusedId).toBe(g.panes[2].id);
    expect(g.zoomedId).toBe(g.panes[1].id);

    const out = serializeWindowSession();
    expect(out.groups[0]).toMatchObject({
      sizes: [0.5, 0.25, 0.25],
      mainFraction: 0.7,
      focused: 2,
      zoomed: 1
    });
  });

  it('omits all four when they are at their defaults', () => {
    useWorkspace.getState().loadSession({
      groups: [{ layout: 'columns', panes: [{ label: 'a' }, { label: 'b' }] }],
      active: 0
    });
    const out = serializeWindowSession();
    const spec = out.groups[0];
    expect('sizes' in spec).toBe(false);
    expect('mainFraction' in spec).toBe(false);
    expect('focused' in spec).toBe(false);
    expect('zoomed' in spec).toBe(false);
  });

  it('falls back safely on invalid sizing / out-of-range indices', () => {
    useWorkspace.getState().loadSession({
      groups: [
        {
          layout: 'columns',
          panes: [{ label: 'a' }, { label: 'b' }],
          sizes: [0.9], // wrong length → equal split
          focused: 5, // out of range → first pane
          zoomed: 9 // out of range → none
        }
      ],
      active: 0
    });
    const g = activeGroup(useWorkspace.getState());
    expect(g.sizes).toEqual([0.5, 0.5]);
    expect(g.focusedId).toBe(g.panes[0].id);
    expect(g.zoomedId).toBeNull();
  });

  it('ignores custom sizes on an auto layout (auto is always equal)', () => {
    useWorkspace.getState().loadSession({
      groups: [{ layout: 'auto', panes: [{ label: 'a' }, { label: 'b' }], sizes: [0.8, 0.2] }],
      active: 0
    });
    expect(activeGroup(useWorkspace.getState()).sizes).toEqual([0.5, 0.5]);
  });
});
