import { describe, expect, it } from 'vitest';
import { activeGroup, useWorkspace } from './store/useWorkspace';
import { useIdle } from './store/useIdle';
import { applyControlCommand, buildControlPayload, settleControlCommand } from './control';

const reset = () =>
  useWorkspace.getState().loadSession({
    groups: [
      { title: 'app', layout: 'columns', panes: [{ label: 'server' }, { label: 'logs' }] }
    ],
    active: 0
  });

describe('buildControlPayload', () => {
  it('snapshots tabs + panes with the active tab id', () => {
    reset();
    const s = useWorkspace.getState();
    const payload = buildControlPayload(s.groups, s.activeId);
    expect(payload.activeTabId).toBe(s.activeId);
    expect(payload.tabs).toHaveLength(1);
    const tab = payload.tabs[0];
    expect(tab).toMatchObject({ id: s.groups[0].id, title: 'app', layout: 'columns' });
    expect(tab.panes.map((p) => p.label)).toEqual(['server', 'logs']);
    // Each pane carries the sessionUid the control API addresses ptys by.
    expect(tab.panes[0].sessionUid).toBe(s.groups[0].panes[0].sessionUid);
  });

  it('derives activity: busy by default, idle when quiet, exited when gone', () => {
    reset();
    useIdle.setState({ idle: {} });
    const s = useWorkspace.getState();
    // No idle flags set → every running pane is busy.
    expect(buildControlPayload(s.groups, s.activeId).tabs[0].panes.map((p) => p.activity)).toEqual([
      'busy',
      'busy'
    ]);

    // Flag the first pane idle (markActivity arms a timer; here we set it directly).
    const paneId = s.groups[0].panes[0].id;
    useIdle.setState((st) => ({ idle: { ...st.idle, [paneId]: true } }));
    expect(
      buildControlPayload(useWorkspace.getState().groups, useWorkspace.getState().activeId).tabs[0]
        .panes[0].activity
    ).toBe('idle');

    // An exited process reports 'exited' regardless of the (still-set) idle flag.
    useWorkspace.getState().markExited(paneId, 0);
    expect(
      buildControlPayload(useWorkspace.getState().groups, useWorkspace.getState().activeId).tabs[0]
        .panes[0].activity
    ).toBe('exited');
  });

  it('includes pane meta, omitting the key when unset', () => {
    reset();
    const target = activeGroup(useWorkspace.getState()).panes[0];
    useWorkspace.getState().setPaneMeta(target.id, { role: 'ceo' });
    const s = useWorkspace.getState();
    const payload = buildControlPayload(s.groups, s.activeId);
    expect(payload.tabs[0].panes[0].meta).toEqual({ role: 'ceo' });
    expect('meta' in payload.tabs[0].panes[1]).toBe(false);
  });

  it('surfaces pane subtitle, omitting the key when unset', () => {
    reset();
    const target = activeGroup(useWorkspace.getState()).panes[0];
    useWorkspace.getState().renamePane(target.id, 'api', 'feature/x');
    const s = useWorkspace.getState();
    const payload = buildControlPayload(s.groups, s.activeId);
    expect(payload.tabs[0].panes[0].subtitle).toBe('feature/x');
    expect('subtitle' in payload.tabs[0].panes[1]).toBe(false);
  });
});

describe('applyControlCommand', () => {
  it('focuses a pane by id', () => {
    reset();
    const target = activeGroup(useWorkspace.getState()).panes[1];
    applyControlCommand({ type: 'focusPane', paneId: target.id });
    expect(activeGroup(useWorkspace.getState()).focusedId).toBe(target.id);
  });

  it('sets a tab layout', () => {
    reset();
    const tabId = useWorkspace.getState().activeId;
    applyControlCommand({ type: 'setLayout', tabId, layout: 'grid' });
    expect(activeGroup(useWorkspace.getState()).layout).toBe('grid');
  });

  it('recolors and renames a pane', () => {
    reset();
    const target = activeGroup(useWorkspace.getState()).panes[0];
    applyControlCommand({ type: 'recolorPane', paneId: target.id, color: '#abc123' });
    applyControlCommand({ type: 'renamePane', paneId: target.id, label: 'api' });
    const after = activeGroup(useWorkspace.getState()).panes.find((p) => p.id === target.id)!;
    expect(after.color).toBe('#abc123');
    expect(after.label).toBe('api');
  });

  it('adds a new pane', () => {
    reset();
    const before = activeGroup(useWorkspace.getState()).panes.length;
    applyControlCommand({ type: 'newPane', pane: { label: 'extra', command: 'top' } });
    const panes = activeGroup(useWorkspace.getState()).panes;
    expect(panes).toHaveLength(before + 1);
    expect(panes.some((p) => p.label === 'extra' && p.command === 'top')).toBe(true);
  });

  it('newPane returns the new pane id and carries spawn-time meta (C/D)', () => {
    reset();
    const id = applyControlCommand({
      type: 'newPane',
      pane: { label: 'worker', command: 'claude', meta: { role: 'worker', parent: 'p0' } }
    });
    expect(typeof id).toBe('string');
    const created = activeGroup(useWorkspace.getState()).panes.find((p) => p.id === id)!;
    expect(created).toBeDefined();
    expect(created.label).toBe('worker');
    expect(created.meta).toEqual({ role: 'worker', parent: 'p0' });
  });

  it('setMeta shallow-merges metadata (new keys win, untouched keys kept)', () => {
    reset();
    const target = activeGroup(useWorkspace.getState()).panes[0];
    applyControlCommand({ type: 'setMeta', paneId: target.id, meta: { role: 'manager', task: 'plan' } });
    applyControlCommand({ type: 'setMeta', paneId: target.id, meta: { task: 'review' } });
    const after = activeGroup(useWorkspace.getState()).panes.find((p) => p.id === target.id)!;
    expect(after.meta).toEqual({ role: 'manager', task: 'review' });
  });

  it('setMeta drops non-string values and ignores an empty result', () => {
    reset();
    const target = activeGroup(useWorkspace.getState()).panes[0];
    // Only the string-valued key survives the metaPatch coercion.
    applyControlCommand({
      type: 'setMeta',
      paneId: target.id,
      meta: { role: 'worker', bad: 5, nested: { x: 1 } } as unknown as Record<string, string>
    });
    const after = activeGroup(useWorkspace.getState()).panes.find((p) => p.id === target.id)!;
    expect(after.meta).toEqual({ role: 'worker' });
  });

  it('setMeta deletes a key on an explicit null value (#6)', () => {
    reset();
    const target = activeGroup(useWorkspace.getState()).panes[0];
    applyControlCommand({ type: 'setMeta', paneId: target.id, meta: { role: 'manager', task: 'plan' } });
    // null clears the stale `task` while keeping the rest.
    applyControlCommand({
      type: 'setMeta',
      paneId: target.id,
      meta: { task: null } as unknown as Record<string, string>
    });
    const after = activeGroup(useWorkspace.getState()).panes.find((p) => p.id === target.id)!;
    expect(after.meta).toEqual({ role: 'manager' });
  });

  it('setMeta drops the meta object entirely once its last key is cleared (#6)', () => {
    reset();
    const target = activeGroup(useWorkspace.getState()).panes[0];
    applyControlCommand({ type: 'setMeta', paneId: target.id, meta: { role: 'worker' } });
    applyControlCommand({
      type: 'setMeta',
      paneId: target.id,
      meta: { role: null } as unknown as Record<string, string>
    });
    const after = activeGroup(useWorkspace.getState()).panes.find((p) => p.id === target.id)!;
    // A fully-cleared pane matches a never-set one (no `meta`).
    expect(after.meta).toBeUndefined();
  });

  it('setMeta returns the TRUE merged meta as the command result, incl. just-set keys (#7 echo race)', () => {
    reset();
    const target = activeGroup(useWorkspace.getState()).panes[0];
    // Seed spawn-time meta, then patch it. The bridge echoes this return value
    // instead of re-reading /state, so the merge must be reflected synchronously.
    applyControlCommand({ type: 'setMeta', paneId: target.id, meta: { role: 'worker', task: 'layer1' } });
    const result = applyControlCommand({
      type: 'setMeta',
      paneId: target.id,
      meta: { status: 'active', task: 'layer2' }
    });
    expect(result).toEqual({ role: 'worker', status: 'active', task: 'layer2' });
  });

  it('setMeta returns {} once the last key is cleared, and undefined for a missing pane (#7)', () => {
    reset();
    const target = activeGroup(useWorkspace.getState()).panes[0];
    applyControlCommand({ type: 'setMeta', paneId: target.id, meta: { role: 'worker' } });
    const cleared = applyControlCommand({
      type: 'setMeta',
      paneId: target.id,
      meta: { role: null } as unknown as Record<string, string>
    });
    expect(cleared).toEqual({});
    expect(applyControlCommand({ type: 'setMeta', paneId: 'nope', meta: { a: 'b' } })).toBeUndefined();
  });

  it('ignores unknown command types', () => {
    reset();
    const snapshot = activeGroup(useWorkspace.getState()).panes.length;
    applyControlCommand({ type: 'nonsense', paneId: 'whatever' });
    expect(activeGroup(useWorkspace.getState()).panes).toHaveLength(snapshot);
  });
});

describe('settleControlCommand', () => {
  it('returns ok with the normalized command result (#9)', async () => {
    reset();
    const reply = await settleControlCommand({ type: 'newPane', pane: { label: 'x' } });
    expect(reply.ok).toBe(true);
    // newPane's result is the new pane id (a string) — not a pending Promise.
    expect(typeof (reply as { ok: true; result?: unknown }).result).toBe('string');
  });

  it('turns a throwing store action into an error reply instead of hanging (#3)', async () => {
    reset();
    const target = activeGroup(useWorkspace.getState()).panes[0];
    const original = useWorkspace.getState().focusPane;
    // A store action that throws would otherwise skip the reply and hang the
    // /command request to its timeout; settleControlCommand reports it instead.
    useWorkspace.setState({
      focusPane: () => {
        throw new Error('boom');
      }
    });
    try {
      const reply = await settleControlCommand({ type: 'focusPane', paneId: target.id });
      expect(reply).toEqual({ ok: false, error: 'boom' });
    } finally {
      useWorkspace.setState({ focusPane: original });
    }
  });
});
