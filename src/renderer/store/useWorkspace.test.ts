import { describe, expect, it } from 'vitest';
import { activeGroup, specFromGroup, useWorkspace } from './useWorkspace';
import { PALETTES } from '../theme';

const panes = () => activeGroup(useWorkspace.getState()).panes;

describe('useWorkspace pane colors', () => {
  it('remapPalette re-slots palette colors and leaves custom colors alone', () => {
    useWorkspace.getState().loadWorkspace({
      panes: [
        { label: 'r', color: PALETTES.dark[0] },
        { label: 'b', color: PALETTES.dark[3] },
        { label: 'custom', color: '#abc123' }
      ]
    });

    useWorkspace.getState().remapPalette('light'); // 'light' = the Neon palette
    const [r, b, c] = panes();
    expect(r.color).toBe(PALETTES.light[0]);
    expect(b.color).toBe(PALETTES.light[3]);
    expect(c.color).toBe('#abc123'); // custom color untouched
  });

  it('loadWorkspace heals a legacy/orphaned color to the active palette', () => {
    // '#f6a8aa' is a pre-vivid "light" pastel — no current palette contains it,
    // but it resolves to slot 0 and is healed to the active (default) palette.
    useWorkspace.getState().loadWorkspace({ panes: [{ label: 'x', color: '#f6a8aa' }] });
    expect(panes()[0].color).toBe(PALETTES.dark[0]);
  });
});

describe('useWorkspace pane subtitle', () => {
  it('renamePane sets, preserves, and clears the subtitle', () => {
    useWorkspace.getState().loadWorkspace({ panes: [{ label: 'a', color: PALETTES.dark[0] }] });
    const id = panes()[0].id;

    useWorkspace.getState().renamePane(id, 'a2', 'feature/x');
    expect(panes()[0]).toMatchObject({ label: 'a2', subtitle: 'feature/x' });

    useWorkspace.getState().renamePane(id, 'a3'); // undefined subtitle → leave as-is
    expect(panes()[0].subtitle).toBe('feature/x');

    useWorkspace.getState().renamePane(id, 'a4', '   '); // blank → clears
    expect(panes()[0].subtitle).toBeUndefined();
  });
});

describe('useWorkspace pane args (direct-spawn argv, P4a)', () => {
  it('addPane stores a direct-spawn args array', () => {
    useWorkspace.getState().loadWorkspace({ panes: [{ label: 'seed' }] });
    const id = useWorkspace.getState().addPane({
      label: 'persona',
      command: 'claude',
      args: ['--append-system-prompt', 'be a pirate, matey']
    });
    const created = panes().find((p) => p.id === id)!;
    expect(created.command).toBe('claude');
    expect(created.args).toEqual(['--append-system-prompt', 'be a pirate, matey']);
  });

  it('round-trips args through specFromGroup → loadSession, dropping it when empty/absent', () => {
    useWorkspace.getState().loadSession({
      groups: [
        {
          title: 't',
          layout: 'auto',
          panes: [
            { label: 'persona', command: 'claude', args: ['--model', 'opus'] },
            { label: 'plain', command: 'top' } // no args
          ]
        }
      ],
      active: 0
    });
    const spec = specFromGroup(activeGroup(useWorkspace.getState()));
    expect(spec.panes[0]).toMatchObject({ command: 'claude', args: ['--model', 'opus'] });
    // A pane without args carries no `args` key (so saved files stay terse).
    expect('args' in spec.panes[1]).toBe(false);
  });

  it('groupFromSpec validates args defensively: drops a non-array or non-string entries', () => {
    useWorkspace.getState().loadSession({
      groups: [
        {
          title: 't',
          panes: [
            { label: 'bad-type', command: 'x', args: 'nope' as unknown as string[] },
            { label: 'mixed', command: 'y', args: ['--flag', 7 as unknown as string, 'value'] },
            { label: 'empty', command: 'z', args: [] }
          ]
        }
      ],
      active: 0
    });
    const [badType, mixed, empty] = panes();
    expect(badType.args).toBeUndefined();
    // Non-string entries are filtered, the survivors kept in order.
    expect(mixed.args).toEqual(['--flag', 'value']);
    expect(empty.args).toBeUndefined();
  });
});

describe('useWorkspace tab actions (context menu)', () => {
  const ws = () => useWorkspace.getState();
  // Replace all tabs with `titles`, each holding one pane, first tab active.
  const setupTabs = (titles: string[]) =>
    ws().loadSession({
      name: 'x',
      panes: [],
      groups: titles.map((t) => ({ title: t, layout: 'auto', panes: [{ label: 'a' }] })),
      active: 0
    });

  it('duplicateGroup inserts a fresh clone right after the source and activates it', () => {
    setupTabs(['one', 'two']);
    const srcId = ws().groups[0].id;
    const srcPaneId = ws().groups[0].panes[0].id;

    ws().duplicateGroup(srcId);
    const s = ws();
    expect(s.groups).toHaveLength(3);
    expect(s.groups[1].title).toBe('one'); // clone sits right after the source
    expect(s.groups[1].id).not.toBe(srcId); // fresh group id
    expect(s.groups[1].panes[0].id).not.toBe(srcPaneId); // fresh pane id → new shell
    expect(s.groups[1].panes[0].sessionUid).not.toBe(ws().groups[0].panes[0].sessionUid);
    expect(s.activeId).toBe(s.groups[1].id); // clone becomes active
  });

  it('closeOthers keeps only the target and stacks the rest for reopen', () => {
    setupTabs(['a', 'b', 'c']);
    const keepId = ws().groups[1].id;
    const before = ws().closed.length;

    ws().closeOthers(keepId);
    const s = ws();
    expect(s.groups).toHaveLength(1);
    expect(s.groups[0].id).toBe(keepId);
    expect(s.activeId).toBe(keepId);
    expect(s.closed.length).toBe(before + 2);
  });

  it('closeToRight keeps 0..i and stacks the tabs to the right (no-op on the last)', () => {
    setupTabs(['a', 'b', 'c', 'd']);
    const before = ws().closed.length;

    ws().closeToRight(ws().groups[1].id); // pivot at index 1
    expect(ws().groups.map((g) => g.title)).toEqual(['a', 'b']);
    expect(ws().closed.length).toBe(before + 2);

    const lastId = ws().groups[ws().groups.length - 1].id;
    ws().closeToRight(lastId); // last tab → nothing to the right
    expect(ws().groups).toHaveLength(2);
  });

  it('setGroupLayout targets the given tab without changing the active tab', () => {
    setupTabs(['a', 'b']);
    const targetId = ws().groups[1].id;
    const activeBefore = ws().activeId;

    ws().setGroupLayout(targetId, 'grid');
    expect(ws().groups[1].layout).toBe('grid');
    expect(ws().activeId).toBe(activeBefore); // focus/active untouched
  });
});
