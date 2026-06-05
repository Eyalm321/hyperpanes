import { describe, expect, it } from 'vitest';
import { useWorkspace } from './useWorkspace';

const ws = () => useWorkspace.getState();
const titles = () => ws().groups.map((g) => g.title);
const labelsAt = (i: number) => ws().groups[i].panes.map((p) => p.label);

// Set up N tabs with the given titles, one pane each; returns their ids in order.
function setupTabs(...names: string[]): string[] {
  ws().loadSession({ panes: [], groups: names.map((t) => ({ title: t, panes: [{ label: t }] })) });
  return ws().groups.map((g) => g.id);
}

// One tab whose panes carry the given labels; returns the group id + pane ids.
function setupOneTab(...labels: string[]): { gid: string; paneIds: string[] } {
  ws().loadSession({ panes: [], groups: [{ title: 'g', panes: labels.map((l) => ({ label: l })) }] });
  const g = ws().groups[0];
  return { gid: g.id, paneIds: g.panes.map((p) => p.id) };
}

describe('moveGroupToIndex (tab reorder)', () => {
  it('moves a tab to a slot among the other tabs', () => {
    const [a] = setupTabs('A', 'B', 'C');
    ws().moveGroupToIndex(a, 2); // A after the other two
    expect(titles()).toEqual(['B', 'C', 'A']);
  });

  it('moves a tab backward (index counts the other tabs only)', () => {
    const [, , c] = setupTabs('A', 'B', 'C');
    ws().moveGroupToIndex(c, 0); // C before the others
    expect(titles()).toEqual(['C', 'A', 'B']);
  });

  it('inserts at the slot among the remaining tabs', () => {
    const [, b] = setupTabs('A', 'B', 'C');
    ws().moveGroupToIndex(b, 2); // others [A,C]; B after both
    expect(titles()).toEqual(['A', 'C', 'B']);
  });

  it('is a no-op when the slot leaves order unchanged', () => {
    const [, b] = setupTabs('A', 'B', 'C');
    const before = ws().groups;
    ws().moveGroupToIndex(b, 1); // others [A,C]; B back between them
    expect(titles()).toEqual(['A', 'B', 'C']);
    expect(ws().groups).toBe(before); // same reference — no re-render
  });

  it('ignores an unknown group id', () => {
    setupTabs('A', 'B');
    ws().moveGroupToIndex('nope', 0);
    expect(titles()).toEqual(['A', 'B']);
  });
});

describe('movePaneToGroup — cross-group (stitch at a slot)', () => {
  it('inserts the pane at the given index in the target and removes it from the source', () => {
    ws().loadSession({
      panes: [],
      groups: [
        { title: 'src', panes: [{ label: 'a' }, { label: 'b' }] },
        { title: 'dst', panes: [{ label: 'x' }, { label: 'y' }] }
      ]
    });
    const a = ws().groups[0].panes[0].id;
    const dst = ws().groups[1].id;
    ws().movePaneToGroup(a, dst, 1);
    expect(labelsAt(0)).toEqual(['b']); // removed from src
    expect(labelsAt(1)).toEqual(['x', 'a', 'y']); // inserted at slot 1
  });

  it('appends when no index is given', () => {
    ws().loadSession({
      panes: [],
      groups: [
        { title: 'src', panes: [{ label: 'a' }] },
        { title: 'dst', panes: [{ label: 'x' }, { label: 'y' }] }
      ]
    });
    const a = ws().groups[0].panes[0].id;
    ws().movePaneToGroup(a, ws().groups[1].id);
    expect(labelsAt(1)).toEqual(['x', 'y', 'a']);
  });
});

describe('movePaneToGroup — same group (reorder)', () => {
  it('moves a pane to a later slot (index counts the original order)', () => {
    const { gid, paneIds } = setupOneTab('a', 'b', 'c');
    ws().movePaneToGroup(paneIds[0], gid, 3); // a → after c
    expect(labelsAt(0)).toEqual(['b', 'c', 'a']);
  });

  it('moves a pane to an earlier slot', () => {
    const { gid, paneIds } = setupOneTab('a', 'b', 'c');
    ws().movePaneToGroup(paneIds[2], gid, 0); // c → front
    expect(labelsAt(0)).toEqual(['c', 'a', 'b']);
  });

  it('keeps the pane (same sessionUid) — a reorder, not a re-create', () => {
    const { gid, paneIds } = setupOneTab('a', 'b', 'c');
    const uidBefore = ws().groups[0].panes[0].sessionUid;
    ws().movePaneToGroup(paneIds[0], gid, 3);
    const moved = ws().groups[0].panes.find((p) => p.id === paneIds[0])!;
    expect(moved.sessionUid).toBe(uidBefore);
  });

  it('is a no-op when the slot resolves to the pane’s own position', () => {
    const { gid, paneIds } = setupOneTab('a', 'b', 'c');
    const before = ws().groups[0];
    ws().movePaneToGroup(paneIds[1], gid, 1); // b → its own slot
    expect(labelsAt(0)).toEqual(['a', 'b', 'c']);
    expect(ws().groups[0]).toBe(before); // unchanged reference
  });
});

describe('injectGroup (dock a tab at a slot)', () => {
  it('inserts a docked group at the given index', () => {
    const ids = setupTabs('A', 'B', 'C');
    const payload = ws().extractGroup(ids[1])!; // remove B → [A, C]
    expect(titles()).toEqual(['A', 'C']);
    ws().injectGroup(payload, 1); // B back at slot 1
    expect(titles()).toEqual(['A', 'B', 'C']);
    expect(ws().activeId).toBe(payload.id);
  });

  it('appends when no index is given', () => {
    const ids = setupTabs('A', 'B', 'C');
    const payload = ws().extractGroup(ids[1])!; // [A, C]
    ws().injectGroup(payload);
    expect(titles()).toEqual(['A', 'C', 'B']);
  });

  it('replaces a pristine empty tab and ignores the index', () => {
    const ids = setupTabs('A', 'B');
    const payload = ws().extractGroup(ids[1])!; // detach B
    ws().loadSession({ panes: [], groups: [{ title: 'empty', panes: [] }] }); // pristine window
    ws().injectGroup(payload, 5);
    expect(ws().groups).toHaveLength(1);
    expect(ws().groups[0].id).toBe(payload.id);
  });

  it('is idempotent — re-docking the same group does not duplicate it', () => {
    const ids = setupTabs('A', 'B', 'C');
    const payload = ws().extractGroup(ids[1])!; // [A, C]
    ws().injectGroup(payload, 1); // [A, B, C]
    ws().injectGroup(payload, 0); // already present → no dup
    expect(titles()).toEqual(['A', 'B', 'C']);
    expect(ws().activeId).toBe(payload.id);
  });
});

describe('adoptPaneInto (cross-window pane stitch)', () => {
  it('inserts an external pane into the target group at the slot and activates it', () => {
    ws().loadSession({
      panes: [],
      groups: [
        { title: 'A', panes: [{ label: 'a1' }, { label: 'a2' }] },
        { title: 'B', panes: [{ label: 'b1' }] }
      ]
    });
    const target = ws().groups[0].id;
    const external = ws().groups[1].panes[0]; // stands in for a pane from another window
    ws().adoptPaneInto(external, target, 1);
    expect(labelsAt(0)).toEqual(['a1', 'b1', 'a2']); // stitched at slot 1
    expect(ws().activeId).toBe(target);
  });

  it('clamps the index and appends past the end', () => {
    ws().loadSession({
      panes: [],
      groups: [
        { title: 'A', panes: [{ label: 'a1' }] },
        { title: 'B', panes: [{ label: 'b1' }] }
      ]
    });
    const external = ws().groups[1].panes[0];
    ws().adoptPaneInto(external, ws().groups[0].id, 99);
    expect(labelsAt(0)).toEqual(['a1', 'b1']);
  });

  it('is idempotent — adopting the same pane twice does not duplicate it', () => {
    ws().loadSession({
      panes: [],
      groups: [
        { title: 'A', panes: [{ label: 'a1' }] },
        { title: 'B', panes: [{ label: 'b1' }] }
      ]
    });
    const target = ws().groups[0].id;
    const external = ws().groups[1].panes[0];
    ws().adoptPaneInto(external, target, 0);
    ws().adoptPaneInto(external, target, 1);
    expect(labelsAt(0)).toEqual(['b1', 'a1']);
  });

  it('ignores an unknown target group', () => {
    const { paneIds } = setupOneTab('a');
    const pane = ws().groups[0].panes[0];
    ws().adoptPaneInto(pane, 'nope', 0);
    expect(ws().groups[0].panes.map((p) => p.id)).toEqual(paneIds);
  });
});

describe('tear-off strips pane.env (scoped token never leaves the window, #5)', () => {
  it('extractGroup hands off panes with env removed', () => {
    ws().loadSession({ panes: [], groups: [{ title: 'g', panes: [] }] });
    const id = ws().addPane({ label: 'worker', env: { HYPERPANES_CONTROL_TOKEN: 'secret' } });
    // The live pane in this window keeps its env (it spawns the pty with it)…
    expect(ws().groups[0].panes.find((p) => p.id === id)!.env).toEqual({
      HYPERPANES_CONTROL_TOKEN: 'secret'
    });
    // …but the torn-off payload that travels to another window must not.
    const payload = ws().extractGroup(ws().groups[0].id)!;
    expect(payload.panes).toHaveLength(1);
    expect(payload.panes[0].env).toBeUndefined();
  });

  it('extractPaneAsGroup strips env from the single torn-out pane', () => {
    ws().loadSession({ panes: [], groups: [{ title: 'g', panes: [] }] });
    const id = ws().addPane({ label: 'worker', env: { TOKEN: 'x' } });
    const payload = ws().extractPaneAsGroup(id)!;
    expect(payload.panes[0].env).toBeUndefined();
  });
});
